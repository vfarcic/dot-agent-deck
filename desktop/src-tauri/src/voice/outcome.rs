//! Consumers 2 and 3 of the table: validation, and the sentence the user reads.
//!
//! **The model returns a situation; the app renders the sentence.** Three
//! reasons, all of which shape this file: the wording stays consistent instead
//! of varying per utterance; a fixture test can assert a sentence, whereas
//! asserting on free-form prose is miserable; and the model cannot invent a
//! plausible-sounding but wrong reason for why something is unavailable,
//! because `screens` already knows.
//!
//! **Every sentence below is rendered here, in Rust — but not all of it comes
//! from the table, and the wider version of that sentence was written here
//! first.** The table supplies two pieces of wording and only two: a row's
//! `report` for a successful dispatch, and its `unavailable_hint` for the
//! not-here refusal. Every other sentence is fixed prose in this file, which is
//! what makes the consistency claim true — it is one file, not one column.
//!
//! **The model writes no user-facing PROSE at any point**, which is the
//! narrower claim and the true one, because two model- or backend-supplied
//! strings DO reach a sentence. A model-supplied **param** is quoted back by
//! the refusals (`no agent here matches “deployer”`) so the user can see what
//! it thought they said, and a backend's own **failure detail** is quoted by
//! [`VoiceOutcome::ResolutionFailed`] and
//! [`VoiceOutcome::TranscriptionFailed`], because "nothing is configured yet"
//! and "the request timed out" are different things to do next. A third
//! arrives by a third route: the agent **labels** an ambiguity sentence lists,
//! and the label a successful `report` interpolates, are the DAEMON's text. All
//! of them are quoted as references, none is wording of the app's own, and
//! every one goes through [`safe_message`] first.
//!
//! **The transcript is the one exception, and it is deliberate**, which is the
//! next paragraph.
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
    /// The name the deck shows for it, which is what the report sentence
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
    /// grammar-constrained backend makes impossible and an unconstrained one
    /// makes merely unlikely — but it renders the **same** sentence, because
    /// from where the user is standing the app did not know how to do what they
    /// asked, which is exactly what a no-match is.
    ///
    /// **Every shipping backend constrains the enum today**, since the
    /// agent-CLI one went: both protocols the keyed backend speaks are
    /// schema-enforced. The variant stays because "this build trusts the
    /// backend" is not a property to acquire by deleting the check, and a user
    /// can point the endpoint at a server that enforces nothing.
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

/// One utterance's outcome, plus what it cost to get it.
///
/// **The latency is produced here rather than left for M6 to invent**, which is
/// PRD #802's mitigation for its own risk entry, and the number it was
/// mitigating was a big one: measured through this very function, the
/// then-default agent-CLI backend took **4.30 s and 6.30 s** — *"slow enough to
/// feel broken"* for a supervisor who just pressed a button — against the keyed
/// backend's 0.62–1.03 s. **That backend is gone, so there is no slow default
/// any more**, which narrows what the seam is for without removing it: both
/// remaining protocols are an HTTP request to whatever endpoint the settings
/// name, and a model on this machine and a hosted API are the same code path,
/// so nothing here can guess what the wait will be. The answer the PRD chose is
/// to surface the number rather than hide it. A surface that had to guess would
/// guess wrong, and a surface handed the real number can say *anthropic, 0.9 s*
/// and let the user decide whether to switch — which is the whole reason the
/// seam is not optional.
///
/// Measured around [`IntentResolver::resolve`], so it is the wall clock the
/// user actually waited for — including the one-time strict-schema compile,
/// which is a real wait that belongs in the number rather than being excused
/// out of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceResult {
    pub outcome: VoiceOutcome,
    /// Milliseconds spent in the backend, or `None` when none was called.
    ///
    /// `None` is a real answer and not a missing one: silence short-circuits
    /// before any backend call, and rendering `0 ms` for it would claim a
    /// measurement that was never taken.
    pub resolve_ms: Option<u32>,
    /// Which backend answered — `anthropic`, `openai`, `stub`.
    ///
    /// The **protocol**, not the vendor: it is rendered beside the latency, and
    /// *remote, 0.9 s* answers a question nobody asked once every backend is
    /// remote. `Protocol::name` produces the first two and the test stub the
    /// third.
    ///
    /// Present even when no call was made, because it names what *would* have
    /// answered, which is what a settings-facing sentence is about.
    pub backend: &'static str,
}

impl VoiceResult {
    /// The sentence to show the user.
    pub fn sentence(&self) -> &str {
        self.outcome.sentence()
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
) -> VoiceResult {
    let backend = resolver.backend_name();
    let finish = |outcome, resolve_ms| VoiceResult {
        outcome,
        resolve_ms,
        backend,
    };

    // Silence is a no-match without a backend call. Every backend is now an
    // HTTP request costing a round trip and, off loopback, money per utterance,
    // and neither is worth spending on an empty string.
    if transcript.is_empty() {
        return finish(VoiceOutcome::no_match(transcript), None);
    }

    let commands = annotate(table, screen);
    let started = std::time::Instant::now();
    let answered = resolver
        .resolve(IntentRequest {
            transcript: &transcript,
            commands: &commands,
            agents,
        })
        .await;
    // Taken before anything is rendered: what the user waited for is the
    // backend, and the table lookups after it are microseconds this must not
    // fold in.
    let resolve_ms = Some(millis(started.elapsed()));
    let finish = move |outcome| finish(outcome, resolve_ms);

    let answer = match answered {
        Ok(answer) => answer,
        Err(error) => return finish(VoiceOutcome::resolution_failed(transcript, &error)),
    };

    if answer.is_no_match() {
        return finish(VoiceOutcome::no_match(transcript));
    }

    // The case PRD #802's four-outcome list could not express: a backend that
    // named an action outside the table. Impossible under grammar-constrained
    // decoding and refused by the schema under either shipping protocol's
    // `strict: true`, but merely unlikely under a backend that constrains
    // nothing — which the withdrawn agent CLI was, and which a server behind a
    // user-supplied endpoint can still be. So it is refused HERE, once, for
    // every backend, rather than trusted to any of them.
    let Some(row) = table.row(&answer.action) else {
        return finish(VoiceOutcome::unknown_action(transcript, answer.action));
    };

    if !row.callable_on(screen) {
        return finish(VoiceOutcome::unavailable(transcript, row));
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
            return finish(VoiceOutcome::ParamMissing {
                sentence: heard(&transcript, spec.kind.missing_phrase()),
                transcript,
                action: row.id.clone(),
                param: spec.name.clone(),
            });
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
                    return finish(VoiceOutcome::ParamUnresolved {
                        sentence: heard(&transcript, &spec.kind.unresolved_phrase(spoken)),
                        transcript,
                        action: row.id.clone(),
                        param: spec.name.clone(),
                        spoken: spoken.to_string(),
                    });
                }
                AgentRefMatch::Ambiguous(labels) => {
                    return finish(VoiceOutcome::ParamAmbiguous {
                        sentence: heard(&transcript, &spec.kind.ambiguous_phrase(spoken, &labels)),
                        transcript,
                        action: row.id.clone(),
                        param: spec.name.clone(),
                        spoken: spoken.to_string(),
                        matches: labels,
                    });
                }
            },
        }
    }

    finish(VoiceOutcome::Dispatch {
        sentence: report(row, &resolved),
        transcript,
        action: row.id.clone(),
        invoke: row.invoke.clone(),
        params: resolved,
    })
}

/// Whole milliseconds, saturating.
///
/// `u32` rather than `u128`: it is a number a surface renders, and 49 days of
/// milliseconds is well past any wait this build permits — both backends are
/// bounded by their own timeout, in seconds. Saturating rather than wrapping so
/// a clock anomaly reads as "very slow" rather than as "instant".
fn millis(elapsed: std::time::Duration) -> u32 {
    u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX)
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

/// The row's `report`, with each `{param}` replaced by what that param
/// resolved to.
///
/// **One pass over the template, deliberately, rather than a `replace` per
/// param.** A label is an agent's display name, which a user chose, so it can
/// contain a `{…}` of its own — and a second `replace` would then substitute
/// into text this function had just inserted. One pass copies a substituted
/// label out and never looks at it again, so what a row renders depends on the
/// template and the labels and not on the order the params happen to be in.
///
/// **The placeholder name is trimmed, because the parse scanner trims and two
/// scanners over one syntax have to agree.** [`super::table`]'s `placeholders`
/// trims before checking a name against the row's declared params, so a report
/// reading `Opening { agent }.` is *accepted* by the table; a byte-exact lookup
/// here then found no param called `" agent "` and rendered the braces to the
/// user on an otherwise successful dispatch — param supplied, resolved,
/// dispatch fine, placeholder unreplaced. The two drifted because no row and no
/// test used the spaced spelling, which is what
/// `voice_outcome_a_report_renders_a_spaced_placeholder` now fixes.
///
/// With them agreed, an unreplaced `{…}` cannot reach a user from a parsed
/// table: every placeholder names a param the row declares
/// (`TableError::UnknownPlaceholder` refuses the rest), and every declared param
/// is in `params` by the time this is called, because the resolution loop above
/// returns a refusal rather than falling through. Anything that is not a
/// well-formed `{name}` is copied through verbatim, which is what a hand-built
/// [`CommandRow`] in a test gets.
fn report(row: &CommandRow, params: &[ResolvedParam]) -> String {
    let mut out = String::with_capacity(row.report.len());
    let mut rest = row.report.as_str();
    while let Some(open) = rest.find('{') {
        let (before, after_open) = rest.split_at(open);
        out.push_str(before);
        let body = &after_open[1..];
        let Some(close) = body.find('}') else {
            // No closing brace at all: the remainder is literal text.
            out.push_str(after_open);
            return out;
        };
        let name = body[..close].trim();
        match params.iter().find(|param| param.name == name) {
            // The label is the DAEMON's text, so it is scrubbed on its way into
            // a sentence for the same reason a refusal's is — see
            // [`ParamKind::unresolved_phrase`].
            Some(param) => out.push_str(&safe_message(&param.label)),
            None => out.push_str(&after_open[..close + 2]),
        }
        rest = &body[close + 1..];
    }
    out.push_str(rest);
    out
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
    ///
    /// **`spoken` is scrubbed, and the transcript beside it deliberately is
    /// not.** This is the MODEL's string — whatever the backend put in its
    /// params object, which is not the same thing as what the transcriber heard
    /// — so it is a foreign string on its way into a sentence that reaches a
    /// DOM node, exactly as a backend's failure detail is, and it gets the same
    /// [`safe_message`]. [`heard`] quotes the transcript verbatim because
    /// seeing exactly what was heard is what turns a mis-transcription into a
    /// correction; that is the one exception and it is a deliberate one.
    fn unresolved_phrase(self, spoken: &str) -> String {
        let spoken = safe_message(spoken);
        match self {
            ParamKind::AgentRef => format!("no agent here matches \u{201c}{spoken}\u{201d}"),
        }
    }

    /// What to say when a value was supplied and more than one thing matches.
    ///
    /// **Both interpolated halves are foreign and both are scrubbed.** `spoken`
    /// is the model's, for [`ParamKind::unresolved_phrase`]'s reason; each label
    /// is the DAEMON's, which under [#741] can be a remote one, so it is the
    /// same class of string and gets the same treatment rather than a different
    /// one for having arrived by a different route.
    ///
    /// [#741]: https://github.com/vfarcic/dot-agent-deck/issues/741
    fn ambiguous_phrase(self, spoken: &str, matches: &[String]) -> String {
        let spoken = safe_message(spoken);
        let shown = matches
            .iter()
            .take(AMBIGUITY_NAMES_SHOWN)
            .map(safe_message)
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
///
/// `pub(super)` since M7's follow-up: [`super::prompt::state`] puts the role
/// beside the label so a model can see the OTHER name the deck shows for an
/// agent, and deriving it twice is how the name the model is shown and the name
/// a spoken reference is matched against would come to disagree.
pub(super) fn role_name(agent: &DesktopAgent) -> Option<String> {
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
///
/// # Invariant: every label this returns is a name [`spoken_names`] answers to
///
/// [`super::schema::TOOL_INSTRUCTIONS`] tells the model to answer a by-state
/// reference — *"the one that's stuck"* — with the agent's **label**. So a
/// label that is not also a spoken name turns a model that did exactly as it
/// was told into a [`VoiceOutcome::ParamUnresolved`] refusal, which is the
/// worst shape of bug available here: correct behaviour punished.
///
/// The first two branches hold it by construction — a display name and a role
/// are both in [`spoken_names`]. The positional fallback does **not**, and it is
/// unreachable only because `crate::dto::map_agent` floors `agent_type` at
/// `"none"`, which makes [`role_name`] `Some` for every agent the app actually
/// produces. `voice_outcome_every_label_is_a_name_the_agent_answers_to` asserts
/// the invariant over those shapes, so a change that lets `agent_type` be
/// genuinely empty goes red there rather than in a user's refusal.
///
/// **Teaching [`spoken_names`] to match positionally was considered and
/// rejected.** "Agent 3" is the agent's place in a snapshot whose order the
/// user does not control and cannot see change, so resolving by it is a worse
/// behaviour than the refusal it would replace. The invariant is pinned
/// instead.
pub(super) fn display_label(agent: &DesktopAgent, agents: &[DesktopAgent]) -> String {
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

/// Whether two names are the same name to a speaker.
///
/// The prompt puts a role or a CLI name beside the label only when it is not
/// already the label, and "not already" has to mean what [`normalize`] means or
/// `claude_code` would be listed beside `Claude Code` as if they were two
/// things to choose between.
pub(super) fn same_spoken_name(one: &str, other: &str) -> bool {
    normalize(one) == normalize(other)
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

    use crate::voice::fixtures::{agent, role_agent};

    fn fleet() -> Vec<DesktopAgent> {
        vec![role_agent("1", "tester"), role_agent("2", "orchestrator")]
    }

    async fn run(
        resolver: &StubResolver,
        screen: Screen,
        agents: &[DesktopAgent],
        said: &str,
    ) -> VoiceOutcome {
        result(resolver, screen, agents, said).await.outcome
    }

    /// The same call, keeping the timing M6 consumes.
    async fn result(
        resolver: &StubResolver,
        screen: Screen,
        agents: &[DesktopAgent],
        said: &str,
    ) -> VoiceResult {
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
        // The report interpolates the DISPLAY name, not what was said, so
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
        // Every shipped row's hint is reachable on some screen, so it is not
        // dead prose — EXCEPT for a row that is callable everywhere, which has
        // no such screen by construction. `voice_table_rows_callable_everywhere_
        // are_the_deliberate_set` pins which rows are in that position; this
        // skips exactly those rather than asserting over a `find` that would
        // panic on them.
        let mut checked = 0;
        for row in table().rows() {
            let Some(screen) = Screen::ALL
                .into_iter()
                .find(|&screen| !row.callable_on(screen))
            else {
                continue;
            };
            let resolver = StubResolver::new().answering("do it", IntentAnswer::new(&row.id));
            let outcome = run(&resolver, screen, &fleet(), "do it").await;
            assert_eq!(
                outcome.sentence(),
                format!("Not here — {}.", row.unavailable_hint)
            );
            checked += 1;
        }
        // And the skip is not the whole table: a `continue` that swallowed every
        // row would leave this test asserting nothing at all.
        assert!(checked >= 4, "only {checked} rows had a hint to render");
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
        // DOM node, so it gets the same scrub the other foreign strings here
        // get — the model's param, and the daemon's labels, which
        // `voice_outcome_scrubs_a_model_supplied_param_and_a_daemon_label`
        // covers. The TRANSCRIPT deliberately does not, because verbatim is the
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

    /// A model-supplied param and a daemon-supplied label are scrubbed too.
    ///
    /// Same class of string as a backend's failure detail, same destination —
    /// a DOM node — and for a while only the detail was scrubbed, which was an
    /// arbitrary asymmetry rather than a decision. The transcript in the same
    /// sentence stays verbatim, which this asserts as well so a later sweep
    /// cannot "fix" the exception away.
    #[tokio::test]
    async fn voice_outcome_scrubs_a_model_supplied_param_and_a_daemon_label() {
        // Unresolved: the param is the model's, and nothing live matches it.
        let resolver = StubResolver::new().answering(
            "open the ghost",
            IntentAnswer::new("open_agent").with_param("agent", "gh\u{7}ost"),
        );
        let outcome = run(&resolver, Screen::Deck, &fleet(), "open the ghost").await;
        assert!(
            !outcome.sentence().contains('\u{7}') && outcome.sentence().contains("ghost"),
            "got {}",
            outcome.sentence()
        );

        // Ambiguous: the param is the model's AND the listed labels are the
        // daemon's. The control character is in the agents' own names as well,
        // because a spoken reference has to MATCH for the ambiguity sentence to
        // be the one that renders.
        let agents = vec![
            role_agent("1", "tes\u{7}ter one"),
            role_agent("2", "tes\u{7}ter two"),
        ];
        let resolver = StubResolver::new().answering(
            "open a tester",
            IntentAnswer::new("open_agent").with_param("agent", "tes\u{7}ter"),
        );
        let outcome = run(&resolver, Screen::Deck, &agents, "open a tester").await;
        assert!(
            matches!(outcome, VoiceOutcome::ParamAmbiguous { .. }),
            "got {outcome:?}"
        );
        assert!(
            !outcome.sentence().contains('\u{7}'),
            "got {}",
            outcome.sentence()
        );
        assert!(
            outcome.sentence().contains("tester one") && outcome.sentence().contains("tester two"),
            "got {}",
            outcome.sentence()
        );

        // And the transcript is still verbatim, control character and all.
        let resolver = StubResolver::new();
        let outcome = run(&resolver, Screen::Deck, &fleet(), "wha\u{7}t").await;
        assert!(
            outcome.sentence().contains('\u{7}'),
            "the transcript was scrubbed: {}",
            outcome.sentence()
        );
    }

    /// Every [`display_label`] output is a name [`spoken_names`] answers to.
    ///
    /// The model is told to answer a by-state reference with the agent's
    /// `label`, so a label outside `spoken_names` refuses a model that obeyed.
    /// Asserted over the shapes `crate::dto::map_agent` actually produces —
    /// including the `agent_type = "none"` floor, which is the single reason
    /// the positional `Agent <n>` fallback is unreachable today.
    #[test]
    fn voice_outcome_every_label_is_a_name_the_agent_answers_to() {
        let shapes = vec![
            (
                "a display name",
                agent("1", Some("Deploy watcher"), "claude_code"),
            ),
            ("a bare agent type", agent("2", None, "claude_code")),
            ("the map_agent floor", agent("3", None, "none")),
            ("an orchestration role", role_agent("4", "orchestrator")),
        ];
        for (what, shaped) in shapes {
            let fleet = vec![shaped.clone()];
            let label = display_label(&shaped, &fleet);
            assert!(
                spoken_names(&shaped)
                    .iter()
                    .any(|name| same_spoken_name(name, &label)),
                "{what}: label {label:?} is not in {:?}",
                spoken_names(&shaped)
            );
            // And the user-visible half of the same property: saying the label
            // back resolves to that agent.
            assert_eq!(
                resolve_agent_ref(&label, &fleet),
                AgentRefMatch::One {
                    id: shaped.id.clone(),
                    label: label.clone(),
                },
                "{what}"
            );
        }
    }

    /// The report template is rendered in ONE pass, so a label cannot be
    /// substituted into.
    ///
    /// An agent's display name is whatever the user renamed it to, so a label
    /// carrying `{…}` is reachable rather than theoretical. Under a
    /// `replace`-per-param render the second param's pass would walk over the
    /// first param's inserted label and rewrite it, making the rendered
    /// sentence depend on the order the params happen to be declared in.
    #[test]
    fn voice_outcome_a_report_renders_in_one_pass_over_its_template() {
        let row = CommandRow {
            id: "two_params".to_string(),
            description: "d".to_string(),
            invoke: "twoParams".to_string(),
            screens: vec![Screen::Deck],
            unavailable_hint: "h".to_string(),
            report: "{first} then {second}.".to_string(),
            params: Vec::new(),
        };
        let param = |name: &str, label: &str| ResolvedParam {
            name: name.to_string(),
            kind: ParamKind::AgentRef,
            spoken: name.to_string(),
            value: name.to_string(),
            label: label.to_string(),
        };

        // The hostile case: the first label names the second param.
        assert_eq!(
            report(
                &row,
                &[param("first", "{second}"), param("second", "tester")],
            ),
            "{second} then tester.",
        );
        // And the ordinary one still renders.
        assert_eq!(
            report(&row, &[param("first", "alpha"), param("second", "beta")]),
            "alpha then beta.",
        );

        // A placeholder with no matching param is copied through verbatim
        // rather than swallowed. A parsed table cannot produce one — that is
        // `TableError::UnknownPlaceholder` — so this covers a hand-built row.
        assert_eq!(
            report(&row, &[param("first", "alpha")]),
            "alpha then {second}."
        );
        // An unclosed brace is likewise literal text, not a panic.
        let unclosed = CommandRow {
            report: "Opening {agent.".to_string(),
            ..row.clone()
        };
        assert_eq!(
            report(&unclosed, &[param("agent", "tester")]),
            "Opening {agent."
        );
    }

    /// A spaced placeholder renders, because the table ACCEPTS one.
    ///
    /// `table::placeholders` trims the name, so `{ agent }` passes the
    /// declared-param check at parse time; this render used to look the name up
    /// byte-exactly and print the braces at the user on a dispatch that had
    /// otherwise succeeded. The absence of exactly this test is why the two
    /// scanners were allowed to drift.
    #[test]
    fn voice_outcome_a_report_renders_a_spaced_placeholder() {
        let row = CommandRow {
            id: "spaced".to_string(),
            description: "d".to_string(),
            invoke: "spaced".to_string(),
            screens: vec![Screen::Deck],
            unavailable_hint: "h".to_string(),
            report: "Opening { agent }.".to_string(),
            params: Vec::new(),
        };
        let param = ResolvedParam {
            name: "agent".to_string(),
            kind: ParamKind::AgentRef,
            spoken: "tester".to_string(),
            value: "1".to_string(),
            label: "tester".to_string(),
        };
        assert_eq!(report(&row, &[param]), "Opening tester.");
    }

    /// The two scanners agree, asserted through a real table rather than a
    /// hand-built row.
    ///
    /// This is the finding's own scenario end to end: a row the TABLE accepts,
    /// resolved by the pipeline, dispatched successfully — and the sentence the
    /// user reads has no braces left in it. A hand-built [`CommandRow`] could
    /// not show this, because what made the bug possible was that
    /// `CommandTable::parse` accepts the spaced spelling in the first place.
    #[tokio::test]
    async fn voice_outcome_a_spaced_placeholder_parses_and_renders() {
        for spelling in ["{agent}", "{ agent}", "{agent }", "{  agent  }"] {
            let source = format!(
                "[[commands]]\n\
                 id = \"open_agent\"\n\
                 invoke = \"openAgent\"\n\
                 description = \"Open one agent\"\n\
                 screens = [\"deck\"]\n\
                 unavailable_hint = \"open the deck first\"\n\
                 report = \"Opening {spelling}.\"\n\
                 params = [{{ name = \"agent\", kind = \"agent_ref\" }}]\n"
            );
            let parsed = CommandTable::parse(&source)
                .unwrap_or_else(|error| panic!("spelling {spelling:?} was refused: {error:?}"));
            let resolver = StubResolver::new().answering(
                "open the tester",
                IntentAnswer::new("open_agent").with_param("agent", "tester"),
            );
            let outcome = handle_utterance(
                &resolver,
                &parsed,
                Screen::Deck,
                &fleet(),
                Transcript::new("open the tester"),
            )
            .await
            .outcome;
            assert!(outcome.is_dispatch(), "spelling {spelling:?}: {outcome:?}");
            assert_eq!(
                outcome.sentence(),
                "Opening tester.",
                "spelling {spelling:?}"
            );
        }
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
                sentence: report(row, std::slice::from_ref(&param)),
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

    // -- what M6 consumes (PRD #802 M5) ------------------------------------

    #[tokio::test]
    async fn voice_outcome_carries_the_latency_the_backend_cost() {
        // PRD #802's mitigation for "slow enough to feel broken" is that the
        // number is produced here rather than invented by the surface. Asserted
        // against a resolver that takes a known, visible amount of time, so
        // this proves the clock is around the BACKEND rather than around
        // nothing.
        struct Slow;
        impl IntentResolver for Slow {
            fn resolve<'a>(
                &'a self,
                _request: IntentRequest<'a>,
            ) -> crate::voice::resolver::ResolveFuture<'a> {
                Box::pin(async {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    Ok(IntentAnswer::new("open_overview"))
                })
            }
            fn backend_name(&self) -> &'static str {
                "slow"
            }
        }
        let result = handle_utterance(
            &Slow,
            table(),
            Screen::Deck,
            &fleet(),
            "show everything".into(),
        )
        .await;
        assert!(result.outcome.is_dispatch(), "{:?}", result.outcome);
        let measured = result.resolve_ms.expect("a backend was called");
        assert!(measured >= 40, "measured {measured}ms for a 40ms backend");
        assert_eq!(result.backend, "slow");
        assert_eq!(result.sentence(), result.outcome.sentence());
    }

    #[tokio::test]
    async fn voice_outcome_reports_no_latency_when_no_backend_was_called() {
        // `None` is a real answer: silence short-circuits, and rendering `0 ms`
        // would claim a measurement never taken. The backend is still named,
        // because that names what WOULD have answered.
        let resolver = StubResolver::failing(IntentError::Backend("never called".into()));
        let result = result(&resolver, Screen::Deck, &fleet(), "   ").await;
        assert!(matches!(result.outcome, VoiceOutcome::NoMatch { .. }));
        assert_eq!(result.resolve_ms, None);
        assert_eq!(result.backend, "stub");
    }

    #[tokio::test]
    async fn voice_outcome_times_a_failing_backend_too() {
        // The failure path is where the number matters most: a user who waited
        // four seconds for "I could not work out what to do" should be able to
        // see that they waited four seconds.
        let resolver = StubResolver::failing(IntentError::Backend("boom".into()));
        let result = result(&resolver, Screen::Deck, &fleet(), "show me the tester").await;
        assert!(matches!(
            result.outcome,
            VoiceOutcome::ResolutionFailed { .. }
        ));
        assert!(result.resolve_ms.is_some());
    }

    #[tokio::test]
    async fn voice_outcome_result_serializes_for_the_webview() {
        let resolver =
            StubResolver::new().answering("go to the overview", IntentAnswer::new("open_overview"));
        let result = result(&resolver, Screen::Deck, &fleet(), "go to the overview").await;
        let json = serde_json::to_value(&result).expect("serializes");
        assert_eq!(json["outcome"]["kind"], "dispatch");
        assert_eq!(json["outcome"]["invoke"], "openOverview");
        assert_eq!(json["backend"], "stub");
        assert!(json["resolveMs"].is_number(), "{json}");
    }
}
