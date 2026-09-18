//! Consumers 2 and 3 of the table: validation, and the sentence the user reads.
//!
//! **The model returns a situation; the app renders the sentence.** Three
//! reasons, all of which shape this file: the wording stays consistent instead
//! of varying per utterance; a fixture test can assert a sentence, whereas
//! asserting on free-form prose is miserable; and the model cannot invent a
//! plausible-sounding but wrong reason for why something is unavailable,
//! because `screens` already knows. Every sentence below is rendered here, in
//! Rust, from the table. The model writes no user-facing PROSE at any point —
//! the narrower claim, and the true one, because a model-supplied param DOES
//! reach a sentence: the refusals quote it back (`no agent here matches
//! “deployer”`) so the user can see what it thought they said. It is quoted
//! as a reference, never as wording of the app's own.
//!
//! **A failure says what it heard.** Most failures are transcription rather
//! than intent, so the transcript goes into the sentence verbatim and turns a
//! dead end into a correction.
//!
//! Nothing here executes anything. A [`VoiceOutcome::Dispatch`] names the
//! `invoke` target and the resolved params; the frontend dispatches it where a
//! click dispatches one (M2/M6).

use std::collections::BTreeSet;

use serde::Serialize;

use super::resolver::{IntentError, IntentRequest, IntentResolver};
use super::schema::annotate;
use super::table::{CommandRow, CommandTable, ParamKind, Screen};
use super::{DesktopAgent, Transcript};
use crate::dto::{DesktopTab, safe_message};

/// How many matching agents an ambiguity sentence names before it summarises.
const AMBIGUITY_NAMES_SHOWN: usize = 3;

/// One param, resolved against live state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedParam {
    pub name: String,
    pub kind: ParamKind,
    /// What the user called it — kept so the surface can show what it matched.
    pub spoken: String,
    /// What it resolved to, and what the frontend dispatches with: an agent id
    /// for [`ParamKind::AgentRef`].
    pub value: String,
    /// The name the deck shows for it, which is what the confirmation sentence
    /// says. Derived the same way the webview derives it, so the sentence names
    /// the agent the way the screen does.
    pub label: String,
}

/// The closed set of situations one utterance can end in.
///
/// Each carries its own rendered `sentence`, so a caller never has to know how
/// to phrase one and no two surfaces can phrase the same situation differently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum VoiceOutcome {
    /// Run this action with these params. The **only** variant that asks for
    /// anything to run — and what runs it is the frontend registry, not this
    /// crate.
    Dispatch {
        transcript: Transcript,
        action: String,
        invoke: String,
        params: Vec<ResolvedParam>,
        sentence: String,
    },
    /// The action exists and the current screen cannot run it. Carries the
    /// table's own hint, which names the prerequisite.
    Unavailable {
        transcript: Transcript,
        action: String,
        hint: String,
        sentence: String,
    },
    /// The model answered "none of these" — the escape that keeps "what time is
    /// it?" from becoming a command.
    NoMatch {
        transcript: Transcript,
        sentence: String,
    },
    /// The model named an action that is not in the table.
    ///
    /// Distinct from [`VoiceOutcome::NoMatch`] because the causes are
    /// different — this one is a backend that did not honour the enum, which a
    /// grammar-constrained backend makes impossible and a print-mode agent CLI
    /// makes merely unlikely — but it renders the **same** sentence, because
    /// from where the user is standing the app did not know how to do what they
    /// asked, which is exactly what a no-match is.
    UnknownAction {
        transcript: Transcript,
        action: String,
        sentence: String,
    },
    /// The action declares a param and the model supplied none.
    ParamMissing {
        transcript: Transcript,
        action: String,
        param: String,
        sentence: String,
    },
    /// A param was supplied and nothing in live state matches it.
    ParamUnresolved {
        transcript: Transcript,
        action: String,
        param: String,
        spoken: String,
        sentence: String,
    },
    /// A param was supplied and more than one thing in live state matches it.
    ParamAmbiguous {
        transcript: Transcript,
        action: String,
        param: String,
        spoken: String,
        matches: Vec<String>,
        sentence: String,
    },
    /// The intent backend could not answer.
    ResolutionFailed {
        transcript: Transcript,
        detail: String,
        sentence: String,
    },
    /// Speech could not be turned into text. Produced by the `Transcriber`
    /// seam (M7), which is upstream of everything else here — so it is the one
    /// variant with no transcript to show.
    TranscriptionFailed { detail: String, sentence: String },
}

impl VoiceOutcome {
    /// The sentence to show the user.
    pub fn sentence(&self) -> &str {
        match self {
            VoiceOutcome::Dispatch { sentence, .. }
            | VoiceOutcome::Unavailable { sentence, .. }
            | VoiceOutcome::NoMatch { sentence, .. }
            | VoiceOutcome::UnknownAction { sentence, .. }
            | VoiceOutcome::ParamMissing { sentence, .. }
            | VoiceOutcome::ParamUnresolved { sentence, .. }
            | VoiceOutcome::ParamAmbiguous { sentence, .. }
            | VoiceOutcome::ResolutionFailed { sentence, .. }
            | VoiceOutcome::TranscriptionFailed { sentence, .. } => sentence,
        }
    }

    /// Whether this outcome asks the frontend to run something.
    pub fn is_dispatch(&self) -> bool {
        matches!(self, VoiceOutcome::Dispatch { .. })
    }

    /// Speech could not be turned into text (M7's seam).
    pub fn transcription_failed(detail: impl AsRef<str>) -> Self {
        let detail = safe_message(detail);
        Self::TranscriptionFailed {
            sentence: format!("Could not turn that into text ({detail})."),
            detail,
        }
    }

    fn no_match(transcript: Transcript) -> Self {
        Self::NoMatch {
            sentence: heard(&transcript, "no matching action"),
            transcript,
        }
    }

    fn unknown_action(transcript: Transcript, action: String) -> Self {
        Self::UnknownAction {
            sentence: heard(&transcript, "no matching action"),
            transcript,
            action,
        }
    }

    fn unavailable(transcript: Transcript, row: &CommandRow) -> Self {
        Self::Unavailable {
            sentence: format!("Not here — {}.", row.unavailable_hint),
            action: row.id.clone(),
            hint: row.unavailable_hint.clone(),
            transcript,
        }
    }

    fn resolution_failed(transcript: Transcript, error: &IntentError) -> Self {
        let detail = safe_message(error.detail());
        Self::ResolutionFailed {
            sentence: heard(
                &transcript,
                &format!("could not work out what to do ({detail})"),
            ),
            transcript,
            detail,
        }
    }
}

/// Take one utterance from a transcript to an outcome.
///
/// The whole middle of the pipeline: annotate the table for the current screen,
/// ask the backend, then refuse anything the table does not sanction — an
/// action that is not in it, an action the current screen cannot run, a missing
/// param, a param that resolves to nothing or to more than one thing. Each
/// refusal is its own outcome carrying its own sentence.
///
/// `table` is a parameter rather than [`super::table::table()`] so a test can
/// drive a fixture table; M6 passes the embedded one.
pub async fn handle_utterance(
    resolver: &dyn IntentResolver,
    table: &CommandTable,
    screen: Screen,
    agents: &[DesktopAgent],
    transcript: Transcript,
) -> VoiceOutcome {
    // Silence is a no-match without a backend call. The default intent backend
    // measured 4.5 s and costs money per utterance, and neither is worth
    // spending on an empty string.
    if transcript.is_empty() {
        return VoiceOutcome::no_match(transcript);
    }

    let commands = annotate(table, screen);
    let answer = match resolver
        .resolve(IntentRequest {
            transcript: &transcript,
            commands: &commands,
            agents,
        })
        .await
    {
        Ok(answer) => answer,
        Err(error) => return VoiceOutcome::resolution_failed(transcript, &error),
    };

    if answer.is_no_match() {
        return VoiceOutcome::no_match(transcript);
    }

    let Some(row) = table.row(&answer.action) else {
        return VoiceOutcome::unknown_action(transcript, answer.action);
    };

    if !row.callable_on(screen) {
        return VoiceOutcome::unavailable(transcript, row);
    }

    // Driven by the ROW's declared params, not by what the model sent, so a
    // param the row does not declare is dropped rather than dispatched. It
    // needs no refusal of its own, because the frontend is handed only params
    // the table declared. (Whether the `invoke` those params travel with names
    // a registry entry that EXISTS is a different question, and not one this
    // function answers — M3's guard is what answers it, at commit time.)
    let mut resolved: Vec<ResolvedParam> = Vec::with_capacity(row.params.len());
    for spec in &row.params {
        let Some(spoken) = answer
            .params
            .get(&spec.name)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        else {
            return VoiceOutcome::ParamMissing {
                sentence: heard(&transcript, spec.kind.missing_phrase()),
                transcript,
                action: row.id.clone(),
                param: spec.name.clone(),
            };
        };
        match spec.kind {
            ParamKind::AgentRef => match resolve_agent_ref(spoken, agents) {
                AgentRefMatch::One { id, label } => resolved.push(ResolvedParam {
                    name: spec.name.clone(),
                    kind: spec.kind,
                    spoken: spoken.to_string(),
                    value: id,
                    label,
                }),
                AgentRefMatch::None => {
                    return VoiceOutcome::ParamUnresolved {
                        sentence: heard(&transcript, &spec.kind.unresolved_phrase(spoken)),
                        transcript,
                        action: row.id.clone(),
                        param: spec.name.clone(),
                        spoken: spoken.to_string(),
                    };
                }
                AgentRefMatch::Ambiguous(labels) => {
                    return VoiceOutcome::ParamAmbiguous {
                        sentence: heard(&transcript, &spec.kind.ambiguous_phrase(spoken, &labels)),
                        transcript,
                        action: row.id.clone(),
                        param: spec.name.clone(),
                        spoken: spoken.to_string(),
                        matches: labels,
                    };
                }
            },
        }
    }

    VoiceOutcome::Dispatch {
        sentence: confirmation(row, &resolved),
        transcript,
        action: row.id.clone(),
        invoke: row.invoke.clone(),
        params: resolved,
    }
}

/// `Heard: “<transcript>” — <situation>.`
///
/// The transcript goes in **verbatim**: seeing exactly what was heard is what
/// turns a mis-transcription into a correction the user can make, so this is
/// not the seam that trims or scrubs it.
fn heard(transcript: &Transcript, situation: &str) -> String {
    format!(
        "Heard: \u{201c}{}\u{201d} — {situation}.",
        transcript.text()
    )
}

/// The row's `confirmation`, with each `{param}` replaced by what that param
/// resolved to.
///
/// A placeholder naming no declared param cannot get here — the table refuses
/// one at parse time — so an unreplaced `{…}` in a rendered sentence would mean
/// a param the model never supplied, which the missing-param refusal catches
/// first.
fn confirmation(row: &CommandRow, params: &[ResolvedParam]) -> String {
    let mut sentence = row.confirmation.clone();
    for param in params {
        sentence = sentence.replace(&format!("{{{}}}", param.name), &param.label);
    }
    sentence
}

impl ParamKind {
    /// What to say when the model picked an action and supplied no value for
    /// this param.
    fn missing_phrase(self) -> &'static str {
        match self {
            ParamKind::AgentRef => "I could not tell which agent you meant",
        }
    }

    /// What to say when a value was supplied and nothing live matches it.
    fn unresolved_phrase(self, spoken: &str) -> String {
        match self {
            ParamKind::AgentRef => format!("no agent here matches \u{201c}{spoken}\u{201d}"),
        }
    }

    /// What to say when a value was supplied and more than one thing matches.
    fn ambiguous_phrase(self, spoken: &str, matches: &[String]) -> String {
        let shown = matches
            .iter()
            .take(AMBIGUITY_NAMES_SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let rest = matches.len().saturating_sub(AMBIGUITY_NAMES_SHOWN);
        let listed = if rest == 0 {
            shown
        } else {
            format!("{shown} and {rest} more")
        };
        match self {
            ParamKind::AgentRef => {
                format!("\u{201c}{spoken}\u{201d} matches more than one agent: {listed}")
            }
        }
    }
}

/// What a spoken agent reference resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRefMatch {
    One { id: String, label: String },
    None,
    Ambiguous(Vec<String>),
}

/// Resolve a spoken reference against the live agent snapshot.
///
/// Two passes, exact before loose, because an exact hit has to win: with agents
/// named "tester" and "tester two", a loose-only match on "tester" would call
/// an unambiguous request ambiguous.
///
/// 1. **Exact** — the normalised reference equals one of the agent's names.
/// 2. **Word subset** — every word of one is present in the other, so "the
///    tester" reaches `tester` and "tester" reaches `Tester agent`. Word sets
///    rather than substrings: under a substring rule a one-letter reference
///    matches every name containing that letter.
///
/// The names an agent answers to are the ones the deck SHOWS for it — its
/// display name, its orchestration role, its agent type and its CLI — plus its
/// id, because ids appear on screen too. Derived here the way
/// `desktop/src/lib/bridge.ts` derives them for display, so a user can say what
/// they can see.
pub fn resolve_agent_ref(spoken: &str, agents: &[DesktopAgent]) -> AgentRefMatch {
    let reference = normalize(spoken);
    if reference.is_empty() {
        return AgentRefMatch::None;
    }
    let reference_words = words(&reference);

    let mut exact: Vec<&DesktopAgent> = Vec::new();
    let mut loose: Vec<&DesktopAgent> = Vec::new();
    for agent in agents {
        let names = spoken_names(agent);
        if names.iter().any(|name| normalize(name) == reference) {
            exact.push(agent);
        } else if names.iter().any(|name| word_subset(&reference_words, name)) {
            loose.push(agent);
        }
    }

    let hits = if exact.is_empty() { loose } else { exact };
    match hits.len() {
        0 => AgentRefMatch::None,
        1 => AgentRefMatch::One {
            id: hits[0].id.clone(),
            label: display_label(hits[0], agents),
        },
        _ => AgentRefMatch::Ambiguous(
            hits.iter()
                .map(|agent| display_label(agent, agents))
                .collect(),
        ),
    }
}

/// Every name this agent answers to.
fn spoken_names(agent: &DesktopAgent) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(display_name) = agent
        .display_name
        .as_ref()
        .filter(|name| !name.trim().is_empty())
    {
        names.push(display_name.clone());
    }
    if let Some(role) = role_name(agent) {
        names.push(role);
    }
    if let Some(cli_name) = agent
        .cli_name
        .as_ref()
        .filter(|name| !name.trim().is_empty())
    {
        names.push(cli_name.clone());
    }
    names.push(agent.id.clone());
    names
}

/// The role, as `bridge.ts`'s `roleFromAgent` derives it: the orchestration
/// role when there is one, the agent type otherwise, underscores spelled as
/// spaces because nobody says "claude underscore code".
fn role_name(agent: &DesktopAgent) -> Option<String> {
    let value = match &agent.tab {
        DesktopTab::Orchestration { role_name, .. } => role_name.clone(),
        _ => agent.agent_type.replace('_', " "),
    };
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// The name the deck shows, which is what a sentence about this agent says.
///
/// `bridge.ts`'s `agentFromDto` is `agent.displayName || role`, with
/// `Agent <n>` when neither is there — the position in the snapshot, the same
/// number the webview uses.
fn display_label(agent: &DesktopAgent, agents: &[DesktopAgent]) -> String {
    if let Some(display_name) = agent
        .display_name
        .as_ref()
        .filter(|name| !name.trim().is_empty())
    {
        return display_name.trim().to_string();
    }
    if let Some(role) = role_name(agent) {
        return role;
    }
    let index = agents
        .iter()
        .position(|candidate| candidate.id == agent.id)
        .unwrap_or(0);
    format!("Agent {}", index + 1)
}

/// Lowercase, with `_` and `-` spelled as spaces and runs of whitespace
/// collapsed — so `claude_code`, `Claude-Code` and `claude  code` are one name.
fn normalize(value: &str) -> String {
    value
        .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

fn words(normalized: &str) -> BTreeSet<String> {
    normalized
        .split(' ')
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether every word of one name is present in the other.
fn word_subset(reference_words: &BTreeSet<String>, name: &str) -> bool {
    let name_words = words(&normalize(name));
    if name_words.is_empty() || reference_words.is_empty() {
        return false;
    }
    name_words.is_subset(reference_words) || reference_words.is_subset(&name_words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::resolver::{IntentAnswer, StubResolver};
    use crate::voice::table::table;

    /// A snapshot agent. Only the fields a spoken reference can reach are
    /// interesting; the rest are what the daemon would have reported.
    fn agent(id: &str, display_name: Option<&str>, agent_type: &str) -> DesktopAgent {
        DesktopAgent {
            id: id.to_string(),
            pane_id: None,
            display_name: display_name.map(str::to_string),
            cwd: None,
            rows: 24,
            cols: 80,
            agent_type: agent_type.to_string(),
            cli_name: None,
            status: "running".to_string(),
            active_tool: None,
            tool_count: 0,
            last_user_prompt: None,
            write_lease: None,
            last_activity_ms: None,
            spawned_at_ms: None,
            tab: DesktopTab::Dashboard,
        }
    }

    fn role_agent(id: &str, role: &str) -> DesktopAgent {
        let mut agent = agent(id, None, "claude_code");
        agent.tab = DesktopTab::Orchestration {
            name: "build".to_string(),
            role_index: 0,
            role_name: role.to_string(),
            is_start_role: false,
            cwd: None,
            display_title: None,
            orchestration_id: None,
        };
        agent
    }

    fn fleet() -> Vec<DesktopAgent> {
        vec![role_agent("1", "tester"), role_agent("2", "orchestrator")]
    }

    async fn run(
        resolver: &StubResolver,
        screen: Screen,
        agents: &[DesktopAgent],
        said: &str,
    ) -> VoiceOutcome {
        handle_utterance(resolver, table(), screen, agents, Transcript::new(said)).await
    }

    // -- dispatch ----------------------------------------------------------

    #[tokio::test]
    async fn voice_outcome_dispatches_a_callable_action_with_a_resolved_param() {
        let resolver = StubResolver::new().answering(
            "show me the tester",
            IntentAnswer::new("open_agent").with_param("agent", "tester"),
        );
        let outcome = run(&resolver, Screen::Deck, &fleet(), "show me the tester").await;
        let VoiceOutcome::Dispatch {
            action,
            invoke,
            params,
            sentence,
            ..
        } = &outcome
        else {
            panic!("expected a dispatch, got {outcome:?}");
        };
        assert_eq!(action, "open_agent");
        assert_eq!(invoke, "openAgent");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "agent");
        assert_eq!(params[0].kind, ParamKind::AgentRef);
        assert_eq!(params[0].spoken, "tester");
        assert_eq!(params[0].value, "1");
        assert_eq!(params[0].label, "tester");
        assert_eq!(sentence, "Opening tester.");
        assert!(outcome.is_dispatch());
    }

    #[tokio::test]
    async fn voice_outcome_dispatches_a_paramless_action() {
        let resolver =
            StubResolver::new().answering("show me everything", IntentAnswer::new("open_overview"));
        let outcome = run(&resolver, Screen::Deck, &fleet(), "show me everything").await;
        assert_eq!(outcome.sentence(), "Opening the agent overview.");
        assert!(outcome.is_dispatch());
    }

    #[tokio::test]
    async fn voice_outcome_drops_a_param_the_row_does_not_declare() {
        // `open_overview` declares none, so a model that volunteers one gets it
        // dropped: the frontend is handed only what the table sanctions.
        let resolver = StubResolver::new().answering(
            "show me everything",
            IntentAnswer::new("open_overview").with_param("agent", "tester"),
        );
        let outcome = run(&resolver, Screen::Deck, &fleet(), "show me everything").await;
        let VoiceOutcome::Dispatch { params, .. } = &outcome else {
            panic!("expected a dispatch, got {outcome:?}");
        };
        assert!(params.is_empty(), "got {params:?}");
    }

    #[tokio::test]
    async fn voice_outcome_dispatch_sentence_names_the_agent_the_deck_shows() {
        // The confirmation interpolates the DISPLAY name, not what was said, so
        // "the one called tester" confirms as the deck spells it.
        let mut agents = fleet();
        agents[0].display_name = Some("Release Tester".to_string());
        let resolver = StubResolver::new().answering(
            "open the tester",
            IntentAnswer::new("open_agent").with_param("agent", "tester"),
        );
        let outcome = run(&resolver, Screen::Deck, &agents, "open the tester").await;
        assert_eq!(outcome.sentence(), "Opening Release Tester.");
    }

    // -- no match and the escape -------------------------------------------

    #[tokio::test]
    async fn voice_outcome_no_match_shows_the_transcript_verbatim() {
        let resolver = StubResolver::new();
        let outcome = run(&resolver, Screen::Deck, &fleet(), "go beck").await;
        assert!(
            matches!(outcome, VoiceOutcome::NoMatch { .. }),
            "got {outcome:?}"
        );
        assert_eq!(
            outcome.sentence(),
            "Heard: \u{201c}go beck\u{201d} — no matching action."
        );
        assert!(
            outcome.sentence().contains("go beck"),
            "the transcript is not verbatim in {}",
            outcome.sentence()
        );
    }

    #[tokio::test]
    async fn voice_outcome_no_match_keeps_odd_transcripts_verbatim() {
        // Verbatim means verbatim: whatever the transducer produced is what the
        // user reads back, including the punctuation and casing that made it
        // wrong.
        let resolver = StubResolver::new();
        for said in [
            "What time is it?",
            "  Open   the TESTER  ",
            "zoom the \"tester\"",
        ] {
            let outcome = run(&resolver, Screen::Deck, &fleet(), said).await;
            assert!(
                outcome.sentence().contains(said),
                "{said:?} is not verbatim in {}",
                outcome.sentence()
            );
        }
    }

    #[tokio::test]
    async fn voice_outcome_silence_is_a_no_match_without_a_backend_call() {
        // A failing resolver proves the backend was not consulted: had it been,
        // this would be a ResolutionFailed.
        let resolver = StubResolver::failing(IntentError::Backend("should not be called".into()));
        let outcome = run(&resolver, Screen::Deck, &fleet(), "   ").await;
        assert!(
            matches!(outcome, VoiceOutcome::NoMatch { .. }),
            "got {outcome:?}"
        );
    }

    // -- validation refusals -----------------------------------------------

    #[tokio::test]
    async fn voice_outcome_refuses_an_action_that_is_not_in_the_table() {
        let resolver =
            StubResolver::new().answering("open the agnet", IntentAnswer::new("open_agnet"));
        let outcome = run(&resolver, Screen::Deck, &fleet(), "open the agnet").await;
        let VoiceOutcome::UnknownAction { action, .. } = &outcome else {
            panic!("expected an unknown action, got {outcome:?}");
        };
        assert_eq!(action, "open_agnet");
        // Same sentence as a no-match: from where the user stands, the app did
        // not know how to do what they asked.
        assert_eq!(
            outcome.sentence(),
            "Heard: \u{201c}open the agnet\u{201d} — no matching action."
        );
    }

    #[tokio::test]
    async fn voice_outcome_refuses_an_action_the_screen_cannot_run_and_carries_the_hint() {
        let resolver = StubResolver::new().answering(
            "open the tester",
            IntentAnswer::new("open_agent").with_param("agent", "tester"),
        );
        let outcome = run(&resolver, Screen::Agent, &fleet(), "open the tester").await;
        let VoiceOutcome::Unavailable { action, hint, .. } = &outcome else {
            panic!("expected unavailable, got {outcome:?}");
        };
        assert_eq!(action, "open_agent");
        assert_eq!(
            hint,
            "opening an agent works from the deck or the agent overview"
        );
        assert_eq!(
            outcome.sentence(),
            "Not here — opening an agent works from the deck or the agent overview."
        );
    }

    #[tokio::test]
    async fn voice_outcome_unavailable_beats_param_resolution() {
        // The screen is checked before the params, so an unavailable action
        // names the prerequisite rather than complaining about an agent.
        let resolver = StubResolver::new().answering(
            "open the ghost",
            IntentAnswer::new("open_agent").with_param("agent", "ghost"),
        );
        let outcome = run(&resolver, Screen::Agent, &fleet(), "open the ghost").await;
        assert!(
            matches!(outcome, VoiceOutcome::Unavailable { .. }),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn voice_outcome_renders_each_rows_hint_on_a_screen_that_cannot_run_it() {
        // Every shipped row's hint is reachable, so none of them is dead prose.
        for row in table().rows() {
            let screen = Screen::ALL
                .into_iter()
                .find(|&screen| !row.callable_on(screen))
                .expect("every row is unavailable somewhere");
            let resolver = StubResolver::new().answering("do it", IntentAnswer::new(&row.id));
            let outcome = run(&resolver, screen, &fleet(), "do it").await;
            assert_eq!(
                outcome.sentence(),
                format!("Not here — {}.", row.unavailable_hint)
            );
        }
    }

    #[tokio::test]
    async fn voice_outcome_refuses_a_missing_param() {
        let resolver = StubResolver::new().answering("open it", IntentAnswer::new("open_agent"));
        let outcome = run(&resolver, Screen::Deck, &fleet(), "open it").await;
        let VoiceOutcome::ParamMissing { action, param, .. } = &outcome else {
            panic!("expected a missing param, got {outcome:?}");
        };
        assert_eq!(action, "open_agent");
        assert_eq!(param, "agent");
        assert_eq!(
            outcome.sentence(),
            "Heard: \u{201c}open it\u{201d} — I could not tell which agent you meant."
        );
    }

    #[tokio::test]
    async fn voice_outcome_treats_a_blank_param_as_missing() {
        let resolver = StubResolver::new().answering(
            "open it",
            IntentAnswer::new("open_agent").with_param("agent", "   "),
        );
        let outcome = run(&resolver, Screen::Deck, &fleet(), "open it").await;
        assert!(
            matches!(outcome, VoiceOutcome::ParamMissing { .. }),
            "got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn voice_outcome_refuses_a_param_that_matches_nothing_live() {
        let resolver = StubResolver::new().answering(
            "open the deployer",
            IntentAnswer::new("open_agent").with_param("agent", "deployer"),
        );
        let outcome = run(&resolver, Screen::Deck, &fleet(), "open the deployer").await;
        let VoiceOutcome::ParamUnresolved { param, spoken, .. } = &outcome else {
            panic!("expected an unresolved param, got {outcome:?}");
        };
        assert_eq!(param, "agent");
        assert_eq!(spoken, "deployer");
        assert_eq!(
            outcome.sentence(),
            "Heard: \u{201c}open the deployer\u{201d} — no agent here matches \u{201c}deployer\u{201d}."
        );
    }

    #[tokio::test]
    async fn voice_outcome_refuses_an_ambiguous_param_and_names_the_candidates() {
        let agents = vec![role_agent("1", "tester one"), role_agent("2", "tester two")];
        let resolver = StubResolver::new().answering(
            "open the tester",
            IntentAnswer::new("open_agent").with_param("agent", "tester"),
        );
        let outcome = run(&resolver, Screen::Deck, &agents, "open the tester").await;
        let VoiceOutcome::ParamAmbiguous {
            matches, spoken, ..
        } = &outcome
        else {
            panic!("expected an ambiguous param, got {outcome:?}");
        };
        assert_eq!(spoken, "tester");
        assert_eq!(
            matches,
            &vec!["tester one".to_string(), "tester two".to_string()]
        );
        assert_eq!(
            outcome.sentence(),
            "Heard: \u{201c}open the tester\u{201d} — \u{201c}tester\u{201d} matches more than one agent: tester one, tester two."
        );
    }

    #[tokio::test]
    async fn voice_outcome_ambiguity_sentence_summarises_a_long_list() {
        let agents: Vec<DesktopAgent> = (1..=5)
            .map(|n| role_agent(&n.to_string(), &format!("tester {n}")))
            .collect();
        let resolver = StubResolver::new().answering(
            "open a tester",
            IntentAnswer::new("open_agent").with_param("agent", "tester"),
        );
        let outcome = run(&resolver, Screen::Deck, &agents, "open a tester").await;
        assert!(
            outcome
                .sentence()
                .ends_with("tester 1, tester 2, tester 3 and 2 more."),
            "got {}",
            outcome.sentence()
        );
    }

    #[tokio::test]
    async fn voice_outcome_reports_a_backend_failure() {
        let resolver = StubResolver::failing(IntentError::NotConfigured(
            "no intent backend is configured".into(),
        ));
        let outcome = run(&resolver, Screen::Deck, &fleet(), "open the tester").await;
        let VoiceOutcome::ResolutionFailed { detail, .. } = &outcome else {
            panic!("expected a resolution failure, got {outcome:?}");
        };
        assert_eq!(detail, "no intent backend is configured");
        assert_eq!(
            outcome.sentence(),
            "Heard: \u{201c}open the tester\u{201d} — could not work out what to do (no intent backend is configured)."
        );
    }

    #[tokio::test]
    async fn voice_outcome_scrubs_control_characters_out_of_a_backend_detail() {
        // A backend's detail is whatever a CLI wrote on stderr. It reaches a
        // DOM node, so it gets the same scrub every other foreign string here
        // gets; the TRANSCRIPT deliberately does not, because verbatim is the
        // point of showing it.
        let resolver = StubResolver::failing(IntentError::Backend("bad\u{7}exit".into()));
        let outcome = run(&resolver, Screen::Deck, &fleet(), "open the tester").await;
        assert!(
            !outcome.sentence().contains('\u{7}'),
            "got {}",
            outcome.sentence()
        );
        assert!(
            outcome.sentence().contains("badexit"),
            "got {}",
            outcome.sentence()
        );
    }

    #[test]
    fn voice_outcome_transcription_failure_has_its_own_sentence() {
        let outcome = VoiceOutcome::transcription_failed("no transcription backend is configured");
        assert_eq!(
            outcome.sentence(),
            "Could not turn that into text (no transcription backend is configured)."
        );
        assert!(!outcome.is_dispatch());
    }

    #[test]
    fn voice_outcome_every_variant_renders_a_sentence() {
        // The property the frontend leans on: whatever happened, there is
        // something to show, and it ends like a sentence.
        let transcript = Transcript::new("go beck");
        let row = table().row("open_agent").expect("present");
        let param = ResolvedParam {
            name: "agent".to_string(),
            kind: ParamKind::AgentRef,
            spoken: "tester".to_string(),
            value: "1".to_string(),
            label: "tester".to_string(),
        };
        let variants = vec![
            VoiceOutcome::Dispatch {
                transcript: transcript.clone(),
                action: row.id.clone(),
                invoke: row.invoke.clone(),
                sentence: confirmation(row, std::slice::from_ref(&param)),
                params: vec![param],
            },
            VoiceOutcome::unavailable(transcript.clone(), row),
            VoiceOutcome::no_match(transcript.clone()),
            VoiceOutcome::unknown_action(transcript.clone(), "nope".to_string()),
            VoiceOutcome::ParamMissing {
                transcript: transcript.clone(),
                action: row.id.clone(),
                param: "agent".to_string(),
                sentence: heard(&transcript, ParamKind::AgentRef.missing_phrase()),
            },
            VoiceOutcome::ParamUnresolved {
                transcript: transcript.clone(),
                action: row.id.clone(),
                param: "agent".to_string(),
                spoken: "ghost".to_string(),
                sentence: heard(&transcript, &ParamKind::AgentRef.unresolved_phrase("ghost")),
            },
            VoiceOutcome::ParamAmbiguous {
                transcript: transcript.clone(),
                action: row.id.clone(),
                param: "agent".to_string(),
                spoken: "tester".to_string(),
                matches: vec!["a".to_string(), "b".to_string()],
                sentence: heard(
                    &transcript,
                    &ParamKind::AgentRef
                        .ambiguous_phrase("tester", &["a".to_string(), "b".to_string()]),
                ),
            },
            VoiceOutcome::resolution_failed(
                transcript.clone(),
                &IntentError::Backend("timed out".into()),
            ),
            VoiceOutcome::transcription_failed("no backend"),
        ];
        for variant in &variants {
            let sentence = variant.sentence();
            assert!(!sentence.is_empty(), "{variant:?} renders nothing");
            assert!(sentence.ends_with('.'), "{sentence:?} is not a sentence");
            assert!(
                !sentence.contains('{'),
                "{sentence:?} has an unreplaced placeholder"
            );
        }
        assert_eq!(
            variants
                .iter()
                .filter(|variant| variant.is_dispatch())
                .count(),
            1
        );
    }

    #[test]
    fn voice_outcome_serializes_with_a_kind_tag_the_webview_can_switch_on() {
        let json = serde_json::to_value(VoiceOutcome::no_match(Transcript::new("go beck")))
            .expect("serializes");
        assert_eq!(json["kind"], "no_match");
        assert_eq!(json["transcript"], "go beck");
        assert_eq!(
            json["sentence"],
            "Heard: \u{201c}go beck\u{201d} — no matching action."
        );
    }

    #[test]
    fn voice_outcome_dispatch_serializes_its_invoke_target_and_params() {
        let outcome = VoiceOutcome::Dispatch {
            transcript: Transcript::new("open the tester"),
            action: "open_agent".to_string(),
            invoke: "openAgent".to_string(),
            params: vec![ResolvedParam {
                name: "agent".to_string(),
                kind: ParamKind::AgentRef,
                spoken: "tester".to_string(),
                value: "1".to_string(),
                label: "tester".to_string(),
            }],
            sentence: "Opening tester.".to_string(),
        };
        let json = serde_json::to_value(&outcome).expect("serializes");
        assert_eq!(json["kind"], "dispatch");
        assert_eq!(json["invoke"], "openAgent");
        assert_eq!(json["params"][0]["value"], "1");
        assert_eq!(json["params"][0]["kind"], "agent_ref");
    }

    // -- param resolution --------------------------------------------------

    #[test]
    fn voice_outcome_agent_ref_matches_a_role_name() {
        let agents = fleet();
        assert_eq!(
            resolve_agent_ref("tester", &agents),
            AgentRefMatch::One {
                id: "1".to_string(),
                label: "tester".to_string()
            }
        );
    }

    #[test]
    fn voice_outcome_agent_ref_ignores_articles_and_case() {
        let agents = fleet();
        for said in ["the tester", "The Tester", "  tester  ", "the tester agent"] {
            assert!(
                matches!(resolve_agent_ref(said, &agents), AgentRefMatch::One { id, .. } if id == "1"),
                "{said:?} did not reach the tester"
            );
        }
    }

    #[test]
    fn voice_outcome_agent_ref_matches_a_display_name_an_agent_type_and_an_id() {
        let mut named = agent("7", Some("Release Tester"), "claude_code");
        named.cli_name = Some("claude".to_string());
        let agents = vec![named];
        for said in [
            "Release Tester",
            "release tester",
            "claude code",
            "claude",
            "7",
        ] {
            assert!(
                matches!(resolve_agent_ref(said, &agents), AgentRefMatch::One { id, .. } if id == "7"),
                "{said:?} did not reach the agent"
            );
        }
    }

    #[test]
    fn voice_outcome_agent_ref_matches_nothing_when_nothing_matches() {
        assert_eq!(resolve_agent_ref("deployer", &fleet()), AgentRefMatch::None);
        assert_eq!(resolve_agent_ref("tester", &[]), AgentRefMatch::None);
        assert_eq!(resolve_agent_ref("   ", &fleet()), AgentRefMatch::None);
    }

    #[test]
    fn voice_outcome_agent_ref_is_ambiguous_when_two_agents_match() {
        let agents = vec![role_agent("1", "tester one"), role_agent("2", "tester two")];
        assert_eq!(
            resolve_agent_ref("tester", &agents),
            AgentRefMatch::Ambiguous(vec!["tester one".to_string(), "tester two".to_string()])
        );
    }

    #[test]
    fn voice_outcome_agent_ref_prefers_an_exact_hit_over_a_loose_one() {
        // "tester" names one agent exactly and is a prefix-word of the other.
        // Without the exact pass this is ambiguous, which is the wrong answer.
        let agents = vec![role_agent("1", "tester"), role_agent("2", "tester two")];
        assert_eq!(
            resolve_agent_ref("tester", &agents),
            AgentRefMatch::One {
                id: "1".to_string(),
                label: "tester".to_string()
            }
        );
    }

    #[test]
    fn voice_outcome_agent_ref_does_not_match_on_a_single_letter() {
        // A substring rule would let "t" reach every agent. Word sets do not.
        let agents = fleet();
        assert_eq!(resolve_agent_ref("t", &agents), AgentRefMatch::None);
    }

    #[test]
    fn voice_outcome_agent_ref_counts_one_agent_once_when_two_of_its_names_match() {
        // Display name and role both reach it; it is still one agent, not an
        // ambiguity.
        let mut both = role_agent("1", "tester");
        both.display_name = Some("tester".to_string());
        assert!(matches!(
            resolve_agent_ref("tester", &[both]),
            AgentRefMatch::One { .. }
        ));
    }

    #[test]
    fn voice_outcome_agent_label_falls_back_the_way_the_webview_does() {
        let unnamed = agent("3", None, "");
        let agents = vec![
            agent("1", None, "claude_code"),
            agent("2", None, ""),
            unnamed,
        ];
        assert_eq!(display_label(&agents[0], &agents), "claude code");
        // No display name and no type: the webview says `Agent <position>`.
        assert_eq!(display_label(&agents[2], &agents), "Agent 3");
    }

    #[test]
    fn voice_outcome_normalize_folds_separators_and_case() {
        assert_eq!(normalize("Claude_Code"), "claude code");
        assert_eq!(normalize("claude-code"), "claude code");
        assert_eq!(normalize("  CLAUDE   code "), "claude code");
        assert_eq!(normalize(" _- "), "");
    }
}
