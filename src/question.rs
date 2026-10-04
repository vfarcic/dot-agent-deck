//! PRD #1542: the question an agent is waiting on, and the answer the deck
//! sends back.
//!
//! The daemon keeps one [`PendingQuestion`] per agent session
//! (`crate::state::SessionState::pending_question`) and carries it on the
//! snapshot (`crate::state::SessionSnapshot::pending_question`). It is built by
//! the agent's own PRODUCER — the deck's hook process, the OpenCode plugin's
//! `await-answer` child, the Pi extension — from what the agent reported, plus
//! the per-agent, per-version option tables in
//! [`crate::agent_registry::AgentSpec::questions`] for options an agent draws
//! on screen but never reports. The deck never reads the screen.
//!
//! A client answers with `AttachRequest::AnswerQuestion`, naming options by
//! index. The daemon validates the answer against the stored question
//! ([`PendingQuestion::validate`]) and answers through one of two channels
//! ([`AnswerChannel`]): **held** — a producer holds a hook-socket connection
//! open for this question and the daemon writes a [`QuestionReply`] down it,
//! which the producer turns into the agent's own decision format
//! ([`claude_decision`], [`opencode_reply`], [`pi_value`]) — or **keys** — the
//! daemon types the keys the option table names ([`answer_keys`]).
//!
//! **Every text field is untrusted agent or model output**
//! (`docs/develop/voice-first-design.md` §6): [`PendingQuestion::sanitized`]
//! caps and strips it on arrival, it is shown verbatim and never interpreted,
//! and a producer maps an answer back onto its OWN copy of the payload by index
//! rather than trusting a label that crossed the socket.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent_registry::{MenuKind, QuestionTables, TableOption, TableWhen};
use crate::event::AgentType;

/// Longest question prompt kept, in bytes.
pub const MAX_PROMPT_BYTES: usize = 500;
/// Longest header or option label kept, in bytes.
pub const MAX_LABEL_BYTES: usize = 120;
/// Longest option description, scope or tool detail kept, in bytes.
pub const MAX_DESCRIPTION_BYTES: usize = 300;
/// Most questions one form may carry; a longer form is cut.
pub const MAX_QUESTIONS: usize = 8;
/// Most options one question may carry; a longer list is cut.
pub const MAX_OPTIONS: usize = 16;
/// Longest question id accepted. An id is a routing key, so an id that is too
/// long or carries anything outside `[A-Za-z0-9_-]` drops the whole question
/// rather than being repaired.
pub const MAX_ID_CHARS: usize = 128;
/// Longest free-text answer a client may send, in characters.
pub const MAX_ANSWER_TEXT_CHARS: usize = 2000;

/// The question an agent is waiting on. Every text field is UNTRUSTED agent or
/// model output: capped by [`Self::sanitized`], shown verbatim, never
/// interpreted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingQuestion {
    /// The agent's own id where it has one (OpenCode's request id, the
    /// `tool_use_id` of Codex's `request_user_input`), else minted by the
    /// producer with [`mint_question_id`] — the Claude hook and the Pi
    /// extension mint it, so the held channel and the snapshot agree on it
    /// without a round trip.
    pub id: String,
    pub kind: QuestionKind,
    /// At least one. One entry for a permission prompt or a single menu; one
    /// per question for a form.
    pub questions: Vec<Question>,
    /// When the producer saw the question, in milliseconds since the Unix
    /// epoch.
    pub raised_at_ms: i64,
    /// What a permission or plan prompt is about. `None` for a choice form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionTool>,
    /// How the daemon will answer. A client reads `Unsupported` as "this one is
    /// answered by keyboard".
    pub channel: AnswerChannel,
    /// Set when the question came from a subagent's event
    /// ([`crate::event::SUBAGENT_ID_METADATA_KEY`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
    /// PRD #1542 (audit A4): the daemon's revision of this question — which
    /// registration of it on its pane an answer is bound to. Minted by the
    /// daemon when it ingests the question (a held question's is its hold's
    /// generation) and never taken from a producer, so a question replaced
    /// under the same id, by another registration or with other content,
    /// carries another revision. A repeat of the pending question with the same
    /// id and content keeps it. A client echoes it on `AnswerQuestion`; the
    /// daemon refuses an answer whose revision is not the pending one, and its
    /// own delivery — the held reply and every typed key — is checked against
    /// it. `None` on a producer's copy, and on the TUI's own state before the
    /// daemon's stamped frame arrives. Additive optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// The tool a permission or plan prompt asks about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The producer's `tool_use_id` when it had one, so the tool's own
    /// `ToolEnd` clears exactly this question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// Allow, always allow or deny a tool use.
    Permission,
    /// Multiple choice: one question or a form.
    Choice,
    /// Claude Code's plan approval (`ExitPlanMode`).
    Plan,
    /// A yes/no confirmation (Pi's `ctx.ui.confirm`).
    Confirm,
    /// A kind this build does not know. Deserialize-only; a client treats it
    /// as keyboard-only.
    #[serde(other)]
    Unknown,
}

/// One question of a form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// In on-screen order.
    pub options: Vec<QuestionOption>,
    /// Several options may be chosen together (Claude's `multiSelect`,
    /// OpenCode's `multiple`). Codex reports no such field.
    #[serde(default)]
    pub multi_select: bool,
}

/// One option of a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// 1-based: the number the agent draws beside it, or the left-to-right
    /// position for a button row with no numbers (OpenCode's permission).
    pub index: u32,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub role: OptionRole,
    /// The deck cannot pick this one — its keys are unknown, or the channel
    /// cannot express it. Shown, never sendable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keyboard_only: bool,
    /// For [`OptionRole::AllowAlways`]: what "always" covers, for the
    /// confirmation the client shows first. May embed untrusted path or
    /// pattern text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionRole {
    AllowOnce,
    AllowAlways,
    Deny,
    Choice,
    /// The user's own words: the answer carries text.
    FreeText,
    /// Deserialize-only; never answerable.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerChannel {
    /// A producer holds a hook-socket connection open for this question.
    Held,
    /// The daemon types the keys the agent's option table names.
    Keys,
    /// The deck cannot answer this question.
    Unsupported,
    /// Deserialize-only; treated as unsupported.
    #[serde(other)]
    Unknown,
}

impl QuestionOption {
    /// Whether a client may send this option, ignoring the channel.
    pub fn answerable(&self) -> bool {
        !self.keyboard_only && self.role != OptionRole::Unknown
    }
}

/// The answer to one question of a form, as a client sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswer {
    /// 0-based into [`PendingQuestion::questions`].
    pub question_index: u32,
    /// [`QuestionOption::index`] values (1-based).
    pub option_indices: Vec<u32>,
    /// Only with a [`OptionRole::FreeText`] option, at most
    /// [`MAX_ANSWER_TEXT_CHARS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// An answer the daemon validated, resolved against the stored question. A
/// producer maps it back onto its own payload by `option_indices`; `labels`
/// and `roles` are the deck's copy, for logs and for producers with no
/// payload of their own to consult.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAnswer {
    pub question_index: u32,
    pub option_indices: Vec<u32>,
    pub labels: Vec<String>,
    pub roles: Vec<OptionRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Why the daemon refused an `AnswerQuestion`. Carried on
/// `AttachResponse::answer_refusal`, so a client never branches on error text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum AnswerRefusal {
    AgentNotFound,
    NoPendingQuestion,
    /// The id named is not the question pending now.
    Stale {
        #[serde(default)]
        current_id: Option<String>,
    },
    /// The answer does not fit the question: an index out of range, a question
    /// missing or answered twice, a single-select question given other than
    /// one option, text without a free-text option or the other way round.
    InvalidAnswer {
        #[serde(default)]
        detail: String,
    },
    /// A chosen option is keyboard-only.
    KeyboardOnly,
    /// An "always" option without the client's confirmation.
    AlwaysNotConfirmed,
    /// The deck cannot answer this question.
    Unsupported,
    /// The held channel closed before the answer reached it.
    ChannelGone,
    /// Typing the keys failed or was refused part way. When at least one key
    /// went in, the question is left pending but keyboard-only.
    WriteFailed {
        #[serde(default)]
        detail: String,
    },
    /// The keys channel would type into a pane the user has typed into since
    /// the question arrived, so the keys could land on a prompt that has
    /// already moved on — a form partly answered by keyboard, or a prompt
    /// already dismissed. The user finishes it by keyboard.
    KeyboardStarted,
    #[serde(other)]
    Unknown,
}

impl std::fmt::Display for AnswerRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentNotFound => write!(f, "no such agent"),
            Self::NoPendingQuestion => write!(f, "no question is pending"),
            Self::Stale { .. } => write!(f, "that question is no longer the pending one"),
            Self::InvalidAnswer { detail } => write!(f, "invalid answer: {detail}"),
            Self::KeyboardOnly => write!(f, "that option has to be answered by keyboard"),
            Self::AlwaysNotConfirmed => write!(f, "always-allow was not confirmed"),
            Self::Unsupported => write!(f, "this question has to be answered by keyboard"),
            Self::ChannelGone => write!(f, "the agent stopped waiting for that answer"),
            Self::WriteFailed { detail } => write!(f, "could not send the answer: {detail}"),
            Self::KeyboardStarted => write!(
                f,
                "the agent's terminal was typed into after it asked; finish the answer by keyboard"
            ),
            Self::Unknown => write!(f, "refused"),
        }
    }
}

/// What the daemon writes down a held connection: one JSON line, then close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionReply {
    pub question_id: String,
    pub outcome: ReplyOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<ResolvedAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ReleaseReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyOutcome {
    Answered,
    Released,
    #[serde(other)]
    Unknown,
}

/// Why a held connection was let go without an answer. Every reason means the
/// same thing to a producer — exit with no decision — and is kept for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    /// A newer question replaced this one.
    Superseded,
    /// The agent moved on (a new turn, an idle, an end).
    Cleared,
    /// The agent reported the question answered by keyboard.
    AnsweredElsewhere,
    Shutdown,
    /// The question was not held: `hold` was false, or it did not survive
    /// sanitizing.
    NotHeld,
    /// The daemon's provenance gate refused the message (issue #1077), so the
    /// event was NOT applied. A producer falls back to sending it as a plain
    /// event, without the question, so the card still shows what happened.
    Refused,
    #[serde(other)]
    Unknown,
}

impl QuestionReply {
    pub fn answered(question_id: &str, answers: Vec<ResolvedAnswer>) -> Self {
        Self {
            question_id: question_id.to_string(),
            outcome: ReplyOutcome::Answered,
            answers,
            reason: None,
        }
    }

    pub fn released(question_id: &str, reason: ReleaseReason) -> Self {
        Self {
            question_id: question_id.to_string(),
            outcome: ReplyOutcome::Released,
            answers: Vec::new(),
            reason: Some(reason),
        }
    }

    /// Whether the daemon refused the message outright, so the event it
    /// carried was not applied ([`ReleaseReason::Refused`]).
    pub fn refused(&self) -> bool {
        self.outcome == ReplyOutcome::Released && self.reason == Some(ReleaseReason::Refused)
    }

    /// The answers, when this reply answers `question_id` — `None` for a
    /// release, or for a reply naming another question, which a producer must
    /// never act on.
    pub fn answers_for(&self, question_id: &str) -> Option<&[ResolvedAnswer]> {
        (self.outcome == ReplyOutcome::Answered && self.question_id == question_id)
            .then_some(self.answers.as_slice())
    }
}

/// Whether `id` may name a question: 1 to [`MAX_ID_CHARS`] of `[A-Za-z0-9_-]`.
pub fn is_valid_question_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A fresh deck-minted question id: `q-` and 32 lowercase hex digits.
pub fn mint_question_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // No OS randomness: fall back to the clock and the pid. Uniqueness
        // among one pane's questions is all an id needs; it is not a secret.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        bytes[..16].copy_from_slice(&(nanos ^ u128::from(std::process::id())).to_le_bytes());
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("q-{hex}")
}

fn clean(raw: &str, max_bytes: usize, keep_newlines: bool) -> String {
    let stripped = crate::untrusted_text::strip_control_and_bidi(raw, keep_newlines);
    crate::prompt_delivery::truncate_on_char_boundary(stripped.trim(), max_bytes)
}

fn clean_opt(raw: Option<String>, max_bytes: usize) -> Option<String> {
    raw.map(|s| clean(&s, max_bytes, false))
        .filter(|s| !s.is_empty())
}

impl PendingQuestion {
    /// The question with every text field capped and stripped of control and
    /// bidi characters, or `None` when it cannot be used at all: a bad id, no
    /// questions, or a question with no options. Applied by the daemon on
    /// arrival, and by every reader of an event, so a hostile producer cannot
    /// put anything else on the snapshot.
    pub fn sanitized(self) -> Option<Self> {
        if !is_valid_question_id(&self.id) {
            return None;
        }
        let mut questions = Vec::new();
        for question in self.questions.into_iter().take(MAX_QUESTIONS) {
            let options: Vec<QuestionOption> = question
                .options
                .into_iter()
                .take(MAX_OPTIONS)
                .map(|option| QuestionOption {
                    index: option.index,
                    label: clean(&option.label, MAX_LABEL_BYTES, false),
                    description: clean_opt(option.description, MAX_DESCRIPTION_BYTES),
                    role: option.role,
                    keyboard_only: option.keyboard_only,
                    scope: clean_opt(option.scope, MAX_DESCRIPTION_BYTES),
                })
                .collect();
            if options.is_empty() {
                return None;
            }
            // Option indices are routing keys too: each must be unique within
            // its question, or an answer could not name one unambiguously.
            let mut seen = std::collections::HashSet::new();
            if !options.iter().all(|o| o.index >= 1 && seen.insert(o.index)) {
                return None;
            }
            questions.push(Question {
                prompt: clean(&question.prompt, MAX_PROMPT_BYTES, true),
                header: clean_opt(question.header, MAX_LABEL_BYTES),
                options,
                multi_select: question.multi_select,
            });
        }
        if questions.is_empty() {
            return None;
        }
        let tool = self.tool.map(|tool| QuestionTool {
            name: clean(&tool.name, MAX_LABEL_BYTES, false),
            detail: clean_opt(tool.detail, MAX_DESCRIPTION_BYTES),
            use_id: tool.use_id.filter(|id| is_valid_question_id(id)),
        });
        Some(Self {
            id: self.id,
            kind: self.kind,
            questions,
            raised_at_ms: self.raised_at_ms,
            tool,
            channel: self.channel,
            subagent_id: clean_opt(self.subagent_id, MAX_LABEL_BYTES),
            revision: self.revision,
        })
    }

    /// PRD #1542 (audit A4): whether `other` is this same question raised
    /// again — every field equal except when it was raised, how the deck would
    /// answer it, and its revision, none of which a repeat of one prompt
    /// changes.
    pub fn same_content(&self, other: &Self) -> bool {
        let bare = |q: &Self| Self {
            raised_at_ms: 0,
            channel: AnswerChannel::Unknown,
            revision: None,
            ..q.clone()
        };
        bare(self) == bare(other)
    }

    /// Whether any option of this question can be answered by the deck at all.
    pub fn has_answerable_option(&self) -> bool {
        matches!(self.channel, AnswerChannel::Held | AnswerChannel::Keys)
            && self
                .questions
                .iter()
                .any(|q| q.options.iter().any(QuestionOption::answerable))
    }

    /// Validate a client's answer against this question and resolve it.
    ///
    /// Refusal order, after the caller has matched the agent and the id: the
    /// whole form first (every question answered exactly once, each choice a
    /// real option of its question, single-select given exactly one, text
    /// only with a free-text option), then keyboard-only options, then the
    /// "always" confirmation, then the channel.
    pub fn validate(
        &self,
        answers: &[QuestionAnswer],
        confirmed_always: bool,
    ) -> Result<Vec<ResolvedAnswer>, AnswerRefusal> {
        let invalid = |detail: String| AnswerRefusal::InvalidAnswer { detail };
        let mut by_question: Vec<Option<&QuestionAnswer>> = vec![None; self.questions.len()];
        for answer in answers {
            let slot = by_question
                .get_mut(answer.question_index as usize)
                .ok_or_else(|| invalid(format!("no question {}", answer.question_index)))?;
            if slot.is_some() {
                return Err(invalid(format!(
                    "question {} answered twice",
                    answer.question_index
                )));
            }
            *slot = Some(answer);
        }
        let mut resolved = Vec::with_capacity(self.questions.len());
        let mut chosen = Vec::new();
        for (index, (question, answer)) in self.questions.iter().zip(&by_question).enumerate() {
            let answer = answer.ok_or_else(|| invalid(format!("question {index} not answered")))?;
            if answer.option_indices.is_empty() {
                return Err(invalid(format!("question {index}: no option chosen")));
            }
            if !question.multi_select && answer.option_indices.len() != 1 {
                return Err(invalid(format!(
                    "question {index} takes exactly one option"
                )));
            }
            let mut options = Vec::with_capacity(answer.option_indices.len());
            for option_index in &answer.option_indices {
                if options
                    .iter()
                    .any(|o: &&QuestionOption| o.index == *option_index)
                {
                    return Err(invalid(format!(
                        "question {index}: option {option_index} chosen twice"
                    )));
                }
                let option = question
                    .options
                    .iter()
                    .find(|o| o.index == *option_index)
                    .ok_or_else(|| {
                        invalid(format!("question {index} has no option {option_index}"))
                    })?;
                options.push(option);
            }
            let free_text = options.iter().any(|o| o.role == OptionRole::FreeText);
            if free_text && options.len() != 1 {
                return Err(invalid(format!(
                    "question {index}: a free-text option is chosen on its own"
                )));
            }
            let text = match (&answer.text, free_text) {
                (Some(text), true) => {
                    let text = crate::untrusted_text::strip_control_and_bidi(text, true);
                    let text = text.trim();
                    if text.is_empty() {
                        return Err(invalid(format!("question {index}: the free text is empty")));
                    }
                    if text.chars().count() > MAX_ANSWER_TEXT_CHARS {
                        return Err(invalid(format!(
                            "question {index}: the free text is over {MAX_ANSWER_TEXT_CHARS} characters"
                        )));
                    }
                    Some(text.to_string())
                }
                (None, true) => {
                    return Err(invalid(format!(
                        "question {index}: a free-text option needs the text"
                    )));
                }
                (Some(_), false) => {
                    return Err(invalid(format!(
                        "question {index}: text is only for a free-text option"
                    )));
                }
                (None, false) => None,
            };
            chosen.extend(options.iter().copied());
            resolved.push(ResolvedAnswer {
                question_index: index as u32,
                option_indices: options.iter().map(|o| o.index).collect(),
                labels: options.iter().map(|o| o.label.clone()).collect(),
                roles: options.iter().map(|o| o.role).collect(),
                text,
            });
        }
        if chosen.iter().any(|o| !o.answerable()) {
            return Err(AnswerRefusal::KeyboardOnly);
        }
        if !confirmed_always && chosen.iter().any(|o| o.role == OptionRole::AllowAlways) {
            return Err(AnswerRefusal::AlwaysNotConfirmed);
        }
        if !matches!(self.channel, AnswerChannel::Held | AnswerChannel::Keys) {
            return Err(AnswerRefusal::Unsupported);
        }
        Ok(resolved)
    }
}

// ---------------------------------------------------------------------------
// Option tables → options
// ---------------------------------------------------------------------------

/// The table options for `menu` that apply under `when`, as question options
/// numbered from `first_index`. A table that does not cover `version` lists its
/// options keyboard-only: the labels are still the best the deck knows, but
/// it does not send keys or decisions it has not verified.
fn table_options(
    tables: Option<&QuestionTables>,
    menu: MenuKind,
    version: Option<(u64, u64, u64)>,
    first_index: u32,
    when: &dyn Fn(TableWhen) -> bool,
    fill: &dyn Fn(&str) -> String,
) -> Vec<QuestionOption> {
    let Some(tables) = tables else {
        return Vec::new();
    };
    let covered = tables.covers(version);
    tables
        .menu(menu)
        .iter()
        .filter(|option| when(option.when))
        .enumerate()
        .map(|(i, option)| table_option(option, first_index + i as u32, covered, fill))
        .collect()
}

fn table_option(
    option: &TableOption,
    index: u32,
    covered: bool,
    fill: &dyn Fn(&str) -> String,
) -> QuestionOption {
    QuestionOption {
        index,
        label: fill(option.label),
        description: None,
        role: option.role,
        keyboard_only: option.keyboard_only || !covered,
        scope: option.scope.map(fill),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The options a payload lists for one question: `options[] = {label,
/// description}`, numbered from 1.
fn payload_options(question: &Value) -> Vec<QuestionOption> {
    question
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    // OpenCode, Claude and Codex all send objects; a bare
                    // string is accepted too, for Pi-shaped lists.
                    let label = option
                        .as_str()
                        .or_else(|| str_field(option, "label"))?
                        .to_string();
                    Some((label, str_field(option, "description").map(str::to_string)))
                })
                .enumerate()
                .map(|(i, (label, description))| QuestionOption {
                    index: i as u32 + 1,
                    label,
                    description,
                    role: OptionRole::Choice,
                    keyboard_only: false,
                    scope: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn agent_tables(agent: &AgentType) -> Option<&'static QuestionTables> {
    crate::agent_registry::spec(agent).questions
}

/// When a table's version range does not cover the agent, a keys channel has
/// nothing verified to type.
fn channel_for(
    tables: Option<&QuestionTables>,
    version: Option<(u64, u64, u64)>,
    channel: AnswerChannel,
) -> AnswerChannel {
    match (channel, tables) {
        (AnswerChannel::Keys, Some(tables)) if !tables.covers(version) => {
            AnswerChannel::Unsupported
        }
        (AnswerChannel::Keys, None) => AnswerChannel::Unsupported,
        (channel, _) => channel,
    }
}

// ---------------------------------------------------------------------------
// Producers: Claude Code
// ---------------------------------------------------------------------------

/// What [`claude_permission_request`] built, and whether the hook should hold
/// its connection open for the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltQuestion {
    pub question: PendingQuestion,
    pub hold: bool,
}

/// The suggestion kind that names Claude Code's option 2, from the first entry
/// of `permission_suggestions` — the one update that option applies.
fn claude_suggestion_when(suggestions: Option<&Value>) -> Option<(TableWhen, Value)> {
    let first = claude_always_update(suggestions)?;
    let when = match str_field(&first, "type") {
        Some("addDirectories") => TableWhen::SuggestionAddDirectories,
        Some("setMode") => TableWhen::SuggestionSetMode,
        _ => TableWhen::SuggestionOther,
    };
    Some((when, first))
}

/// PRD #1542 (audit A2): the ONE permission update Claude Code's "always"
/// option stands for — the first `permission_suggestions` entry, whose kind
/// also chooses the option's label. The held hook sends exactly this update
/// as `updatedPermissions` ([`claude_decision`]), and the confirmation the user
/// answers is written from it ([`claude_update_scope`]), so what is confirmed
/// and what is granted are the same thing. A later suggestion is never sent:
/// nothing on screen or in the confirmation describes it.
pub fn claude_always_update(suggestions: Option<&Value>) -> Option<Value> {
    suggestions?.as_array()?.first().cloned()
}

/// Where a Claude Code permission update is kept, as the end of a sentence —
/// `None` for a destination this build does not know, which the caller treats
/// as "cannot describe it".
fn claude_destination(update: &Value) -> Option<&'static str> {
    Some(match str_field(update, "destination")? {
        "session" => "for the rest of this session",
        "localSettings" => "in this project's local settings, for later sessions too",
        "projectSettings" => "in this project's shared settings, for everyone working in it",
        "userSettings" => "in your user settings, for every project",
        _ => return None,
    })
}

/// A Claude Code permission mode in words, for the label and the confirmation.
/// `None` for a mode this build does not know.
fn claude_mode(mode: &str) -> Option<(&'static str, &'static str)> {
    Some(match mode {
        "acceptEdits" => (
            "accept edits",
            "accept-edits mode: file edits without asking",
        ),
        "bypassPermissions" => (
            "bypass permissions",
            "bypass-permissions mode: every tool without asking",
        ),
        "plan" => ("plan mode", "plan mode"),
        "default" => ("the default mode", "the default permission mode"),
        "dontAsk" => (
            "don't ask",
            "don't-ask mode: anything not already allowed is denied",
        ),
        _ => return None,
    })
}

/// Whether `text` can be shown in a confirmation exactly as it is granted:
/// non-empty, already trimmed, nothing [`PendingQuestion::sanitized`] would
/// strip (control or bidi characters), and no `,` — the separator the scope
/// joins its targets with, so a target containing one would read as two.
fn claude_scope_part_is_exact(text: &str) -> bool {
    !text.is_empty()
        && text.trim() == text
        && !text.contains(',')
        && crate::untrusted_text::strip_control_and_bidi(text, false) == text
}

/// PRD #1542 (audit A2): what one Claude Code permission update grants, in
/// words, for the "always allow" confirmation — written from the update's own
/// `type`, `mode`, `destination`, `directories` and `rules`, never from a
/// table. `None` when the update is anything this build cannot describe
/// faithfully (an unknown type, mode or destination, a rule that is not an
/// allow, an empty list) or cannot show COMPLETELY: a target the snapshot's
/// sanitizing would alter or that reads ambiguously
/// ([`claude_scope_part_is_exact`]), or a whole sentence longer than
/// [`MAX_DESCRIPTION_BYTES`], which the snapshot would cut. The option is then
/// keyboard-only, and [`claude_decision`] refuses to send it — so no grant
/// ever reaches past what the confirmation displayed.
pub fn claude_update_scope(update: &Value) -> Option<String> {
    let scope = claude_update_scope_words(update)?;
    (scope.len() <= MAX_DESCRIPTION_BYTES && clean(&scope, MAX_DESCRIPTION_BYTES, false) == scope)
        .then_some(scope)
}

fn claude_update_scope_words(update: &Value) -> Option<String> {
    let destination = claude_destination(update)?;
    let list = |key: &str| -> Option<Vec<&str>> {
        let items: Vec<&str> = update
            .get(key)?
            .as_array()?
            .iter()
            .map(|item| item.as_str().filter(|s| claude_scope_part_is_exact(s)))
            .collect::<Option<_>>()?;
        (!items.is_empty()).then_some(items)
    };
    match str_field(update, "type")? {
        "addDirectories" => Some(format!(
            "access to {} {destination}",
            list("directories")?.join(", ")
        )),
        "setMode" => {
            let (_, words) = claude_mode(str_field(update, "mode")?)?;
            Some(format!("switching to {words}, {destination}"))
        }
        "addRules" => {
            if str_field(update, "behavior")? != "allow" {
                return None;
            }
            let rules = update
                .get("rules")?
                .as_array()?
                .iter()
                .map(|rule| {
                    let tool =
                        str_field(rule, "toolName").filter(|t| claude_scope_part_is_exact(t))?;
                    Some(match str_field(rule, "ruleContent") {
                        Some(content) if claude_scope_part_is_exact(content) => {
                            format!("{tool}({content})")
                        }
                        Some(_) => return None,
                        None => tool.to_string(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            if rules.is_empty() {
                return None;
            }
            Some(format!(
                "{} without asking, {destination}",
                rules.join(", ")
            ))
        }
        _ => None,
    }
}

/// Fill a Claude permission label's `{dir}` / `{mode}` placeholders from the
/// suggestion that names it.
fn claude_suggestion_fill(suggestion: &Value, template: &str) -> String {
    let dirs = suggestion
        .get("directories")
        .and_then(Value::as_array)
        .map(|dirs| {
            dirs.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let mode = str_field(suggestion, "mode")
        .map(|mode| claude_mode(mode).map_or(mode, |(label, _)| label))
        .unwrap_or_default();
    template.replace("{dir}", &dirs).replace("{mode}", mode)
}

/// PRD #1542: the question a Claude Code `PermissionRequest` hook payload asks.
///
/// - `AskUserQuestion` → a [`QuestionKind::Choice`] form, one question per
///   `tool_input.questions[]`, the payload's options followed by the options
///   Claude Code appends on screen; held.
/// - `ExitPlanMode` → [`QuestionKind::Plan`], options from the table; answered
///   by keys, NOT held — a hook decision does not dismiss the plan dialog
///   [observed on 2.1.289].
/// - any other tool → [`QuestionKind::Permission`]: Yes, the "always" option
///   named by the payload's first `permission_suggestions` entry when there is
///   one — its scope written from that entry, or keyboard-only when it cannot
///   be described — and No; held.
///
/// `version` is the Claude Code version when known; the hook does not know it,
/// and passes `None`.
pub fn claude_permission_request(
    id: String,
    tool_name: &str,
    tool_input: Option<&Value>,
    tool_detail: Option<String>,
    permission_suggestions: Option<&Value>,
    raised_at_ms: i64,
    version: Option<(u64, u64, u64)>,
) -> BuiltQuestion {
    let tables = agent_tables(&AgentType::ClaudeCode);
    let no_fill = |s: &str| s.to_string();
    match tool_name {
        "AskUserQuestion" => {
            let questions = tool_input
                .and_then(|input| input.get("questions"))
                .and_then(Value::as_array)
                .map(|questions| {
                    questions
                        .iter()
                        .map(|question| {
                            let mut options = payload_options(question);
                            let next = options.len() as u32 + 1;
                            options.extend(table_options(
                                tables,
                                MenuKind::ChoiceAppended,
                                version,
                                next,
                                &|_| true,
                                &no_fill,
                            ));
                            Question {
                                prompt: str_field(question, "question").unwrap_or("").to_string(),
                                header: str_field(question, "header").map(str::to_string),
                                options,
                                multi_select: question
                                    .get("multiSelect")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            BuiltQuestion {
                question: PendingQuestion {
                    id,
                    kind: QuestionKind::Choice,
                    questions,
                    raised_at_ms,
                    // Named so the tool's own `ToolEnd` clears the form when
                    // the keyboard answers it.
                    tool: Some(QuestionTool {
                        name: tool_name.to_string(),
                        detail: None,
                        use_id: None,
                    }),
                    channel: AnswerChannel::Held,
                    subagent_id: None,
                    revision: None,
                },
                hold: true,
            }
        }
        "ExitPlanMode" => {
            let options = table_options(tables, MenuKind::Plan, version, 1, &|_| true, &no_fill);
            BuiltQuestion {
                question: PendingQuestion {
                    id,
                    kind: QuestionKind::Plan,
                    questions: vec![Question {
                        prompt: "Approve Claude's plan?".to_string(),
                        header: None,
                        options,
                        multi_select: false,
                    }],
                    raised_at_ms,
                    tool: Some(QuestionTool {
                        name: tool_name.to_string(),
                        detail: None,
                        use_id: None,
                    }),
                    channel: channel_for(tables, version, AnswerChannel::Keys),
                    subagent_id: None,
                    revision: None,
                },
                hold: false,
            }
        }
        _ => {
            let suggestion = claude_suggestion_when(permission_suggestions);
            let suggestion_value = suggestion.as_ref().map(|(_, v)| v.clone());
            let wanted = suggestion.as_ref().map(|(when, _)| *when);
            let fill = |template: &str| match &suggestion_value {
                Some(value) => claude_suggestion_fill(value, template),
                None => template.to_string(),
            };
            let mut options = table_options(
                tables,
                MenuKind::Permission,
                version,
                1,
                &|when| when == TableWhen::Always || Some(when) == wanted,
                &fill,
            );
            // The confirmation names exactly what the decision will send, or
            // the option cannot be sent at all (audit A2).
            let scope = suggestion_value.as_ref().and_then(claude_update_scope);
            for option in options
                .iter_mut()
                .filter(|o| o.role == OptionRole::AllowAlways)
            {
                match &scope {
                    Some(scope) => option.scope = Some(scope.clone()),
                    None => option.keyboard_only = true,
                }
            }
            BuiltQuestion {
                question: PendingQuestion {
                    id,
                    kind: QuestionKind::Permission,
                    questions: vec![Question {
                        prompt: format!("Allow {tool_name}?"),
                        header: None,
                        options,
                        multi_select: false,
                    }],
                    raised_at_ms,
                    tool: Some(QuestionTool {
                        name: tool_name.to_string(),
                        detail: tool_detail,
                        use_id: None,
                    }),
                    channel: AnswerChannel::Held,
                    subagent_id: None,
                    revision: None,
                },
                hold: true,
            }
        }
    }
}

/// The `hookSpecificOutput` a held Claude Code `PermissionRequest` hook prints
/// for `reply`, or `None` when it must print nothing (a release, a reply for
/// another question, or an answer it cannot map onto the payload).
///
/// - Permission: allow-once → `allow`; always → `allow` with
///   `updatedPermissions` = the ONE update the option stands for, the payload's
///   first `permission_suggestions` entry verbatim ([`claude_always_update`];
///   [observed for `setMode`]), and nothing when [`claude_update_scope`] cannot
///   describe it; deny → `deny` with a message.
/// - Choice: `allow` with `updatedInput` = the payload's `tool_input` with
///   `answers` added — `updatedInput` REPLACES the input, so `questions` must
///   ride along unchanged [observed]. A single-select answer is the label, a
///   multi-select one an array of labels, a free-text one the text; labels come
///   from the payload by index, never from the reply.
pub fn claude_decision(
    question: &PendingQuestion,
    tool_input: Option<&Value>,
    permission_suggestions: Option<&Value>,
    reply: &QuestionReply,
) -> Option<Value> {
    let answers = reply.answers_for(&question.id)?;
    let decision = match question.kind {
        QuestionKind::Permission => {
            let role = answers.first()?.roles.first().copied()?;
            match role {
                OptionRole::AllowOnce => serde_json::json!({"behavior": "allow"}),
                OptionRole::AllowAlways => {
                    // Exactly the one update the option stands for, and only
                    // one the confirmation could describe (audit A2).
                    let update = claude_always_update(permission_suggestions)?;
                    claude_update_scope(&update)?;
                    serde_json::json!({
                        "behavior": "allow",
                        "updatedPermissions": [update],
                    })
                }
                OptionRole::Deny => serde_json::json!({
                    "behavior": "deny",
                    "message": "The user declined this request.",
                }),
                _ => return None,
            }
        }
        QuestionKind::Choice => {
            let input = tool_input?.as_object()?;
            let payload_questions = input.get("questions")?.as_array()?;
            let mut map = serde_json::Map::new();
            for answer in answers {
                let payload_question = payload_questions.get(answer.question_index as usize)?;
                let key = str_field(payload_question, "question")?.to_string();
                let payload_labels: Vec<String> = payload_options(payload_question)
                    .into_iter()
                    .map(|o| o.label)
                    .collect();
                let value = if let Some(text) = &answer.text {
                    Value::String(text.clone())
                } else {
                    let labels = answer
                        .option_indices
                        .iter()
                        .map(|i| payload_labels.get((*i as usize).checked_sub(1)?).cloned())
                        .collect::<Option<Vec<_>>>()?;
                    let multi = question
                        .questions
                        .get(answer.question_index as usize)
                        .is_some_and(|q| q.multi_select);
                    if multi {
                        Value::Array(labels.into_iter().map(Value::String).collect())
                    } else {
                        Value::String(labels.into_iter().next()?)
                    }
                };
                map.insert(key, value);
            }
            let mut updated = input.clone();
            updated.insert("answers".to_string(), Value::Object(map));
            serde_json::json!({"behavior": "allow", "updatedInput": Value::Object(updated)})
        }
        _ => return None,
    };
    Some(serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": decision,
        }
    }))
}

// ---------------------------------------------------------------------------
// Producers: Codex
// ---------------------------------------------------------------------------

/// PRD #1542: a Codex `PermissionRequest` → a [`QuestionKind::Permission`]
/// answered by keys from Codex's command-approval table. Never held: a running
/// `PermissionRequest` hook hides Codex's prompt from a keyboard user
/// [observed on 0.160.0].
pub fn codex_permission_request(
    id: String,
    tool_name: &str,
    tool_detail: Option<String>,
    raised_at_ms: i64,
    version: Option<(u64, u64, u64)>,
) -> PendingQuestion {
    let tables = agent_tables(&AgentType::Codex);
    let detail = tool_detail.clone().unwrap_or_default();
    let options = table_options(
        tables,
        MenuKind::Permission,
        version,
        1,
        &|_| true,
        &|template: &str| template.replace("{detail}", &detail),
    );
    PendingQuestion {
        id,
        kind: QuestionKind::Permission,
        questions: vec![Question {
            prompt: format!("Allow {tool_name}?"),
            header: None,
            options,
            multi_select: false,
        }],
        raised_at_ms,
        tool: Some(QuestionTool {
            name: tool_name.to_string(),
            detail: tool_detail,
            use_id: None,
        }),
        channel: channel_for(tables, version, AnswerChannel::Keys),
        subagent_id: None,
        revision: None,
    }
}

/// PRD #1542: Codex's `request_user_input` tool, from its `PreToolUse` →
/// a [`QuestionKind::Choice`] form answered by one digit per question. The id
/// is the call's `tool_use_id`, so its `PostToolUse` clears exactly this form.
/// Codex reports no multi-select field [observed], so none is single-select.
pub fn codex_request_user_input(
    tool_use_id: Option<&str>,
    tool_input: Option<&Value>,
    raised_at_ms: i64,
    version: Option<(u64, u64, u64)>,
) -> Option<PendingQuestion> {
    let tables = agent_tables(&AgentType::Codex);
    let questions = tool_input?
        .get("questions")?
        .as_array()?
        .iter()
        .map(|question| {
            let mut options = payload_options(question);
            let next = options.len() as u32 + 1;
            options.extend(table_options(
                tables,
                MenuKind::ChoiceAppended,
                version,
                next,
                &|_| true,
                &|s: &str| s.to_string(),
            ));
            Question {
                prompt: str_field(question, "question").unwrap_or("").to_string(),
                header: str_field(question, "header").map(str::to_string),
                options,
                multi_select: false,
            }
        })
        .collect();
    let id = tool_use_id
        .filter(|id| is_valid_question_id(id))
        .map(str::to_string)
        .unwrap_or_else(mint_question_id);
    Some(PendingQuestion {
        id,
        kind: QuestionKind::Choice,
        questions,
        raised_at_ms,
        tool: Some(QuestionTool {
            name: "request_user_input".to_string(),
            detail: None,
            use_id: tool_use_id.map(str::to_string),
        }),
        channel: channel_for(tables, version, AnswerChannel::Keys),
        subagent_id: None,
        revision: None,
    })
}

// ---------------------------------------------------------------------------
// Producers: Devin (from its documentation; untested)
// ---------------------------------------------------------------------------

/// PRD #1542: a Devin `PermissionRequest` → a [`QuestionKind::Permission`]
/// whose options come from a table built from Devin's documentation. Only
/// Allow once and Deny are answerable, by keys that no logged-in Devin has
/// verified. Not held: whether a running hook hides Devin's prompt is unknown.
pub fn devin_permission_request(
    id: String,
    tool_name: &str,
    tool_detail: Option<String>,
    raised_at_ms: i64,
    version: Option<(u64, u64, u64)>,
) -> PendingQuestion {
    let tables = agent_tables(&AgentType::Devin);
    let options = table_options(
        tables,
        MenuKind::Permission,
        version,
        1,
        &|_| true,
        &|s: &str| s.to_string(),
    );
    PendingQuestion {
        id,
        kind: QuestionKind::Permission,
        questions: vec![Question {
            prompt: format!("Allow {tool_name}?"),
            header: None,
            options,
            multi_select: false,
        }],
        raised_at_ms,
        tool: Some(QuestionTool {
            name: tool_name.to_string(),
            detail: tool_detail,
            use_id: None,
        }),
        channel: channel_for(tables, version, AnswerChannel::Keys),
        subagent_id: None,
        revision: None,
    }
}

// ---------------------------------------------------------------------------
// Producers: OpenCode (the plugin's `await-answer` child)
// ---------------------------------------------------------------------------

/// PRD #1542: OpenCode's `permission.asked` properties → a held
/// [`QuestionKind::Permission`] with OpenCode's three fixed replies. The id is
/// OpenCode's own request id, so its `permission.replied` names it.
pub fn opencode_permission_asked(props: &Value, raised_at_ms: i64) -> Option<PendingQuestion> {
    let id = str_field(props, "id")?.to_string();
    let permission = str_field(props, "permission").unwrap_or("permission");
    let patterns = |key: &str| {
        props
            .get(key)
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    };
    let detail = props
        .get("metadata")
        .and_then(|m| str_field(m, "command"))
        .map(str::to_string)
        .or_else(|| Some(patterns("patterns")).filter(|p| !p.is_empty()));
    let always = patterns("always");
    let tables = agent_tables(&AgentType::OpenCode);
    let options = table_options(
        tables,
        MenuKind::Permission,
        None,
        1,
        &|_| true,
        &|template: &str| template.replace("{patterns}", &always),
    );
    Some(PendingQuestion {
        id,
        kind: QuestionKind::Permission,
        questions: vec![Question {
            prompt: format!("Allow {permission}?"),
            header: None,
            options,
            multi_select: false,
        }],
        raised_at_ms,
        tool: Some(QuestionTool {
            name: permission.to_string(),
            detail,
            use_id: None,
        }),
        channel: AnswerChannel::Held,
        subagent_id: None,
        revision: None,
    })
}

/// PRD #1542: OpenCode's `question.asked` properties → a held
/// [`QuestionKind::Choice`] form. Each question gets OpenCode's own
/// "Type your own answer" option unless it says `custom: false`.
pub fn opencode_question_asked(props: &Value, raised_at_ms: i64) -> Option<PendingQuestion> {
    let id = str_field(props, "id")?.to_string();
    let tables = agent_tables(&AgentType::OpenCode);
    let questions = props
        .get("questions")?
        .as_array()?
        .iter()
        .map(|question| {
            let mut options = payload_options(question);
            if question.get("custom").and_then(Value::as_bool) != Some(false) {
                let next = options.len() as u32 + 1;
                options.extend(table_options(
                    tables,
                    MenuKind::ChoiceAppended,
                    None,
                    next,
                    &|_| true,
                    &|s: &str| s.to_string(),
                ));
            }
            Question {
                prompt: str_field(question, "question").unwrap_or("").to_string(),
                header: str_field(question, "header").map(str::to_string),
                options,
                multi_select: question
                    .get("multiple")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        })
        .collect();
    Some(PendingQuestion {
        id,
        kind: QuestionKind::Choice,
        questions,
        raised_at_ms,
        tool: None,
        channel: AnswerChannel::Held,
        subagent_id: None,
        revision: None,
    })
}

/// What the OpenCode plugin does with an answer: POST `body` to
/// `/permission/{request_id}/reply` or `/question/{request_id}/reply`.
///
/// Permission: allow-once → `once`, always → `always`, deny → `reject`. A form:
/// `answers` = one array of labels per question, in question order, labels
/// taken from the payload by index; a free-text answer is the text as the
/// array's one element [unverified — the lane-2 test pins it].
pub fn opencode_reply(
    question: &PendingQuestion,
    props: &Value,
    reply: &QuestionReply,
) -> Option<Value> {
    let answers = reply.answers_for(&question.id)?;
    match question.kind {
        QuestionKind::Permission => {
            let reply = match answers.first()?.roles.first()? {
                OptionRole::AllowOnce => "once",
                OptionRole::AllowAlways => "always",
                OptionRole::Deny => "reject",
                _ => return None,
            };
            Some(serde_json::json!({
                "kind": "permission",
                "request_id": question.id,
                "body": {"reply": reply},
            }))
        }
        QuestionKind::Choice => {
            let payload_questions = props.get("questions")?.as_array()?;
            let mut out = vec![Value::Null; question.questions.len()];
            for answer in answers {
                let index = answer.question_index as usize;
                let value = if let Some(text) = &answer.text {
                    vec![Value::String(text.clone())]
                } else {
                    let labels: Vec<String> = payload_options(payload_questions.get(index)?)
                        .into_iter()
                        .map(|o| o.label)
                        .collect();
                    answer
                        .option_indices
                        .iter()
                        .map(|i| {
                            labels
                                .get((*i as usize).checked_sub(1)?)
                                .cloned()
                                .map(Value::String)
                        })
                        .collect::<Option<Vec<_>>>()?
                };
                *out.get_mut(index)? = Value::Array(value);
            }
            if out.iter().any(Value::is_null) {
                return None;
            }
            Some(serde_json::json!({
                "kind": "question",
                "request_id": question.id,
                "body": {"answers": out},
            }))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Producers: Pi (the deck's extension wrapping another extension's dialog)
// ---------------------------------------------------------------------------

/// The dialog the Pi extension saw another extension raise, as it describes it
/// to `await-answer`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PiDialog {
    pub id: String,
    /// `select`, `confirm` or `input`.
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
}

/// PRD #1542: a Pi dialog → a held question. `select` → a choice of its
/// options; `confirm` → [`QuestionKind::Confirm`] with Yes (allow) and No
/// (deny); `input` → one free-text option.
pub fn pi_dialog(dialog: &PiDialog, raised_at_ms: i64) -> Option<PendingQuestion> {
    let option = |index: u32, label: &str, role: OptionRole| QuestionOption {
        index,
        label: label.to_string(),
        description: None,
        role,
        keyboard_only: false,
        scope: None,
    };
    let (kind, prompt, options) = match dialog.kind.as_str() {
        "select" => (
            QuestionKind::Choice,
            dialog.title.clone(),
            dialog
                .options
                .iter()
                .enumerate()
                .map(|(i, label)| option(i as u32 + 1, label, OptionRole::Choice))
                .collect(),
        ),
        "confirm" => (
            QuestionKind::Confirm,
            match &dialog.message {
                Some(message) if !message.is_empty() => format!("{}\n{message}", dialog.title),
                _ => dialog.title.clone(),
            },
            vec![
                option(1, "Yes", OptionRole::AllowOnce),
                option(2, "No", OptionRole::Deny),
            ],
        ),
        "input" => (
            QuestionKind::Choice,
            dialog.title.clone(),
            vec![option(
                1,
                dialog
                    .placeholder
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .unwrap_or("Your answer"),
                OptionRole::FreeText,
            )],
        ),
        _ => return None,
    };
    Some(PendingQuestion {
        id: dialog.id.clone(),
        kind,
        questions: vec![Question {
            prompt,
            header: None,
            options,
            multi_select: false,
        }],
        raised_at_ms,
        tool: None,
        channel: AnswerChannel::Held,
        subagent_id: None,
        revision: None,
    })
}

/// The value the Pi extension resolves the asking extension's dialog with:
/// `{"value": <the option string> | true | false | <text>}`. The option string
/// is taken from the dialog's own list by index.
pub fn pi_value(
    dialog: &PiDialog,
    question: &PendingQuestion,
    reply: &QuestionReply,
) -> Option<Value> {
    let answer = reply.answers_for(&question.id)?.first()?;
    let value = match dialog.kind.as_str() {
        "select" => {
            let index = (*answer.option_indices.first()? as usize).checked_sub(1)?;
            Value::String(dialog.options.get(index)?.clone())
        }
        "confirm" => match answer.roles.first()? {
            OptionRole::AllowOnce => Value::Bool(true),
            OptionRole::Deny => Value::Bool(false),
            _ => return None,
        },
        "input" => Value::String(answer.text.clone()?),
        _ => return None,
    };
    Some(serde_json::json!({ "value": value }))
}

// ---------------------------------------------------------------------------
// The keys channel
// ---------------------------------------------------------------------------

/// The key sequences that answer `question` for an agent of `agent`, one entry
/// per write — the daemon re-checks between writes that the question is still
/// pending. For a form, one digit per question in question order: on Codex a
/// digit answers the current question and moves to the next, and the last one
/// submits the form [observed on 0.160.0].
///
/// `Err(Unsupported)` when the agent's table names no keys for a chosen option.
pub fn answer_keys(
    agent: &AgentType,
    question: &PendingQuestion,
    answers: &[ResolvedAnswer],
) -> Result<Vec<String>, AnswerRefusal> {
    let tables = agent_tables(agent).ok_or(AnswerRefusal::Unsupported)?;
    match question.kind {
        QuestionKind::Permission | QuestionKind::Plan => {
            let menu = if question.kind == QuestionKind::Plan {
                MenuKind::Plan
            } else {
                MenuKind::Permission
            };
            let answer = answers.first().ok_or(AnswerRefusal::Unsupported)?;
            let index = *answer
                .option_indices
                .first()
                .ok_or(AnswerRefusal::Unsupported)?;
            let option = question
                .questions
                .first()
                .and_then(|q| q.options.iter().find(|o| o.index == index))
                .ok_or(AnswerRefusal::Unsupported)?;
            // Each answerable role appears once in a keyed menu (the table
            // tests pin it), so the chosen option's role names its entry.
            let keys = tables
                .menu(menu)
                .iter()
                .find(|entry| entry.role == option.role && entry.keys.is_some())
                .and_then(|entry| entry.keys)
                .ok_or(AnswerRefusal::Unsupported)?;
            Ok(vec![keys.to_string()])
        }
        QuestionKind::Choice => {
            let mut ordered: Vec<&ResolvedAnswer> = answers.iter().collect();
            ordered.sort_by_key(|a| a.question_index);
            ordered
                .into_iter()
                .map(|answer| match answer.option_indices.as_slice() {
                    [index] if (1..=9).contains(index) && answer.text.is_none() => {
                        Ok(index.to_string())
                    }
                    _ => Err(AnswerRefusal::Unsupported),
                })
                .collect()
        }
        _ => Err(AnswerRefusal::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    fn option(index: u32, label: &str, role: OptionRole) -> QuestionOption {
        QuestionOption {
            index,
            label: label.to_string(),
            description: None,
            role,
            keyboard_only: false,
            scope: None,
        }
    }

    fn permission(channel: AnswerChannel) -> PendingQuestion {
        PendingQuestion {
            id: "q-1".to_string(),
            kind: QuestionKind::Permission,
            questions: vec![Question {
                prompt: "Allow Bash?".to_string(),
                header: None,
                options: vec![
                    option(1, "Yes", OptionRole::AllowOnce),
                    QuestionOption {
                        scope: Some("commands like touch".to_string()),
                        ..option(2, "Always", OptionRole::AllowAlways)
                    },
                    option(3, "No", OptionRole::Deny),
                ],
                multi_select: false,
            }],
            raised_at_ms: 1_700_000_000_000,
            tool: Some(QuestionTool {
                name: "Bash".to_string(),
                detail: Some("touch x".to_string()),
                use_id: None,
            }),
            channel,
            subagent_id: None,
            revision: None,
        }
    }

    fn form() -> PendingQuestion {
        PendingQuestion {
            id: "q-form".to_string(),
            kind: QuestionKind::Choice,
            questions: vec![
                Question {
                    prompt: "Which colour?".to_string(),
                    header: Some("Colour".to_string()),
                    options: vec![
                        option(1, "Red", OptionRole::Choice),
                        option(2, "Green", OptionRole::Choice),
                        option(3, "Type something.", OptionRole::FreeText),
                        QuestionOption {
                            keyboard_only: true,
                            ..option(4, "Chat about this", OptionRole::Choice)
                        },
                    ],
                    multi_select: false,
                },
                Question {
                    prompt: "Which sizes?".to_string(),
                    header: Some("Sizes".to_string()),
                    options: vec![
                        option(1, "Small", OptionRole::Choice),
                        option(2, "Large", OptionRole::Choice),
                    ],
                    multi_select: true,
                },
            ],
            raised_at_ms: 1,
            tool: None,
            channel: AnswerChannel::Held,
            subagent_id: None,
            revision: None,
        }
    }

    fn answer(question_index: u32, option_indices: &[u32], text: Option<&str>) -> QuestionAnswer {
        QuestionAnswer {
            question_index,
            option_indices: option_indices.to_vec(),
            text: text.map(str::to_string),
        }
    }

    /// Scenario: A pending question with every optional field set is written
    /// to JSON and read back unchanged; a snapshot written without a pending
    /// question omits the key, and one read without it holds no question.
    #[spec("question/model/001")]
    #[test]
    fn question_model_001_round_trips_and_is_additive_on_the_snapshot() {
        let mut question = form();
        question.tool = Some(QuestionTool {
            name: "AskUserQuestion".into(),
            detail: Some("d".into()),
            use_id: Some("toolu_1".into()),
        });
        question.subagent_id = Some("sub-1".into());
        let json = serde_json::to_string(&question).unwrap();
        let back: PendingQuestion = serde_json::from_str(&json).unwrap();
        assert_eq!(back, question);
        // A keyboard-only flag of `false` is not written at all.
        assert!(
            !serde_json::to_string(&permission(AnswerChannel::Held))
                .unwrap()
                .contains("keyboard_only")
        );

        let snapshot = serde_json::json!({"status": "Idle", "tool_count": 0});
        let decoded: crate::state::SessionSnapshot =
            serde_json::from_value(snapshot).expect("an older daemon's snapshot decodes");
        assert_eq!(decoded.pending_question, None);
        let encoded = serde_json::to_value(&decoded).unwrap();
        assert!(
            encoded.get("pending_question").is_none(),
            "an absent question must not be written: {encoded}"
        );
        let mut with = decoded.clone();
        with.pending_question = Some(question.clone());
        let encoded = serde_json::to_string(&with).unwrap();
        let back: crate::state::SessionSnapshot = serde_json::from_str(&encoded).unwrap();
        assert_eq!(back.pending_question, Some(question));
    }

    /// Scenario: A question from a newer build names a kind, a channel and an
    /// option role this build has never heard of. It still decodes, each
    /// unknown value reads as Unknown, and the Unknown option is never
    /// answerable, nor is anything on an Unknown channel.
    #[spec("question/model/002")]
    #[test]
    fn question_model_002_unknown_values_decode_and_are_never_answerable() {
        let json = serde_json::json!({
            "id": "q-new",
            "kind": "survey",
            "questions": [{
                "prompt": "p",
                "options": [
                    {"index": 1, "label": "a", "role": "allow_forever"},
                    {"index": 2, "label": "b", "role": "choice"}
                ]
            }],
            "raised_at_ms": 1,
            "channel": "telepathy"
        });
        let question: PendingQuestion = serde_json::from_value(json).unwrap();
        assert_eq!(question.kind, QuestionKind::Unknown);
        assert_eq!(question.channel, AnswerChannel::Unknown);
        assert_eq!(question.questions[0].options[0].role, OptionRole::Unknown);
        assert!(!question.questions[0].options[0].answerable());
        assert!(question.questions[0].options[1].answerable());
        assert!(!question.has_answerable_option());
        assert_eq!(
            question.validate(&[answer(0, &[1], None)], false),
            Err(AnswerRefusal::KeyboardOnly)
        );
        assert_eq!(
            question.validate(&[answer(0, &[2], None)], false),
            Err(AnswerRefusal::Unsupported)
        );
        let refusal: AnswerRefusal =
            serde_json::from_value(serde_json::json!({"code": "something_new"})).unwrap();
        assert_eq!(refusal, AnswerRefusal::Unknown);
    }

    /// Scenario: A hostile producer sends a question whose text carries escape
    /// sequences, bidi overrides and kilobytes of padding, with too many
    /// questions and options. Sanitizing caps and strips every field; a bad
    /// id, an empty option list or a duplicated option index drops the question.
    #[spec("question/model/003")]
    #[test]
    fn question_model_003_sanitizing_caps_strips_and_drops() {
        let mut question = form();
        question.questions[0].prompt = format!("\x1b[2JWhich\u{202e} colour?{}", "x".repeat(2000));
        question.questions[0].options[0].label = format!("Red\x07{}", "r".repeat(500));
        question.questions[0].options[0].description = Some("d".repeat(1000));
        question.questions[0].header = Some("\u{0085}Colour".into());
        for i in 0..20 {
            question.questions[1]
                .options
                .push(option(10 + i, "more", OptionRole::Choice));
        }
        for _ in 0..10 {
            question.questions.push(question.questions[1].clone());
        }
        let clean = question.clone().sanitized().expect("still a question");
        let first = &clean.questions[0];
        assert!(
            first.prompt.starts_with("[2JWhich colour?"),
            "{}",
            first.prompt
        );
        assert!(first.prompt.len() <= MAX_PROMPT_BYTES + '…'.len_utf8());
        assert!(first.options[0].label.starts_with("Redr"));
        assert!(first.options[0].label.len() <= MAX_LABEL_BYTES + '…'.len_utf8());
        assert!(
            first.options[0].description.as_ref().unwrap().len()
                <= MAX_DESCRIPTION_BYTES + '…'.len_utf8()
        );
        assert_eq!(first.header.as_deref(), Some("Colour"));
        assert_eq!(clean.questions.len(), MAX_QUESTIONS);
        assert_eq!(clean.questions[1].options.len(), MAX_OPTIONS);
        for text in clean
            .questions
            .iter()
            .flat_map(|q| std::iter::once(&q.prompt).chain(q.options.iter().map(|o| &o.label)))
        {
            assert!(
                !text.chars().any(|c| c.is_control() && c != '\n'),
                "{text:?}"
            );
        }

        let mut bad_id = form();
        bad_id.id = "q 1;rm".into();
        assert_eq!(bad_id.sanitized(), None);
        let mut long_id = form();
        long_id.id = "q".repeat(MAX_ID_CHARS + 1);
        assert_eq!(long_id.sanitized(), None);
        let mut empty = form();
        empty.questions[1].options.clear();
        assert_eq!(empty.sanitized(), None);
        let mut duplicate = form();
        duplicate.questions[1].options[1].index = 1;
        assert_eq!(duplicate.sanitized(), None);
        let mut none = form();
        none.questions.clear();
        assert_eq!(none.sanitized(), None);
    }

    /// Scenario: Claude Code's captured Bash permission prompt carries two
    /// suggestions — access to a directory, then accept-edits mode. The user
    /// confirms "always allow" against the sentence the deck shows, and the
    /// decision the hook prints grants exactly that one update and nothing the
    /// sentence does not name; an update the deck cannot describe makes the
    /// option keyboard-only and produces no decision.
    #[spec("question/hold/005")]
    #[test]
    fn question_hold_005_always_allow_grants_exactly_what_was_confirmed() {
        let payload: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/agent-questions/claude-permission-bash.json"
        ))
        .unwrap();
        let suggestions = payload.get("permission_suggestions");
        assert_eq!(
            suggestions.and_then(Value::as_array).map(Vec::len),
            Some(2),
            "the fixture is the two-suggestion case"
        );
        let question = claude_permission_request(
            "q-a2".into(),
            "Bash",
            payload.get("tool_input"),
            Some("touch created_m1.txt".into()),
            suggestions,
            1,
            None,
        )
        .question;
        let always = question.questions[0]
            .options
            .iter()
            .find(|o| o.role == OptionRole::AllowAlways)
            .expect("an always option");
        assert!(always.answerable());
        let confirmed = always.scope.clone().expect("a scope to confirm");
        let reply = QuestionReply::answered(
            "q-a2",
            question
                .validate(&[answer(0, &[always.index], None)], true)
                .unwrap(),
        );
        let decision = claude_decision(&question, payload.get("tool_input"), suggestions, &reply)
            .expect("a decision");
        let sent = decision["hookSpecificOutput"]["decision"]["updatedPermissions"]
            .as_array()
            .expect("updatedPermissions")
            .clone();
        let described: Vec<String> = sent
            .iter()
            .map(|u| claude_update_scope(u).expect("every update sent is describable"))
            .collect();
        assert_eq!(
            described,
            vec![confirmed.clone()],
            "the confirmed scope is exactly what is granted"
        );
        assert_eq!(sent, vec![payload["permission_suggestions"][0].clone()]);
        assert_eq!(
            confirmed,
            "access to /work/proj for the rest of this session"
        );
        assert!(
            !serde_json::to_string(&sent)
                .unwrap()
                .contains("acceptEdits")
        );

        // Per-mode and per-destination wording.
        let set_mode = |mode: &str, destination: &str| {
            claude_update_scope(&serde_json::json!({
                "type": "setMode", "mode": mode, "destination": destination
            }))
        };
        assert_eq!(
            set_mode("acceptEdits", "session").as_deref(),
            Some(
                "switching to accept-edits mode: file edits without asking, for the rest of this session"
            )
        );
        assert!(
            set_mode("bypassPermissions", "userSettings")
                .unwrap()
                .contains("every tool without asking, in your user settings")
        );
        assert_eq!(set_mode("warpSpeed", "session"), None);
        assert_eq!(set_mode("acceptEdits", "somewhere"), None);
        let label = |mode: &str| {
            claude_permission_request(
                "q-m".into(),
                "Write",
                None,
                None,
                Some(&serde_json::json!([{"type": "setMode", "mode": mode, "destination": "session"}])),
                1,
                None,
            )
            .question
            .questions[0]
            .options[1]
            .clone()
        };
        assert_eq!(
            label("acceptEdits").label,
            "Yes, and switch to accept edits for this session"
        );
        assert_eq!(
            label("bypassPermissions").label,
            "Yes, and switch to bypass permissions for this session"
        );

        // An update the deck cannot describe: shown, never sendable, and no
        // decision even if a reply claimed it.
        let odd =
            serde_json::json!([{"type": "replaceRules", "rules": [], "destination": "session"}]);
        let question =
            claude_permission_request("q-odd".into(), "Bash", None, None, Some(&odd), 1, None)
                .question;
        let always = &question.questions[0].options[1];
        assert_eq!(always.role, OptionRole::AllowAlways);
        assert!(always.keyboard_only && always.scope.is_none());
        let forged = QuestionReply::answered(
            "q-odd",
            vec![ResolvedAnswer {
                question_index: 0,
                option_indices: vec![2],
                labels: vec![always.label.clone()],
                roles: vec![OptionRole::AllowAlways],
                text: None,
            }],
        );
        assert_eq!(claude_decision(&question, None, Some(&odd), &forged), None);
    }

    /// Scenario: Claude Code suggests "always allow" for an update too long, or
    /// too odd, to show whole — many directories whose broadest one sits past
    /// what the card can show, many rules, a path with a hidden bidi character
    /// or one containing the comma the deck joins targets with. Each reaches the
    /// card through the event's own sanitizing as a keyboard-only option with
    /// nothing to confirm, and a reply claiming it sends no decision, so no
    /// grant reaches a target the confirmation did not show.
    #[spec("question/hold/010")]
    #[test]
    fn question_hold_010_an_always_allow_too_long_to_show_whole_is_keyboard_only() {
        let through_event = |suggestions: &Value| -> (PendingQuestion, QuestionOption) {
            let built = claude_permission_request(
                "q-long".into(),
                "Bash",
                None,
                Some("ls".into()),
                Some(suggestions),
                1,
                None,
            )
            .question;
            let mut event: crate::event::AgentEvent = serde_json::from_value(serde_json::json!({
                "session_id": "s-long",
                "agent_type": "claude_code",
                "event_type": "waiting_for_input",
                "timestamp": "2026-10-04T10:00:00Z",
            }))
            .unwrap();
            event.set_question(&built);
            let shown = event.question().expect("the question survives sanitizing");
            let always = shown.questions[0]
                .options
                .iter()
                .find(|o| o.role == OptionRole::AllowAlways)
                .expect("an always option")
                .clone();
            (shown, always)
        };
        let forged = |question: &PendingQuestion, always: &QuestionOption| {
            QuestionReply::answered(
                &question.id,
                vec![ResolvedAnswer {
                    question_index: 0,
                    option_indices: vec![always.index],
                    labels: vec![always.label.clone()],
                    roles: vec![OptionRole::AllowAlways],
                    text: None,
                }],
            )
        };

        // The broadest directory and the destination fall past the cap.
        let mut directories: Vec<String> = (0..6)
            .map(|i| format!("/work/a-rather-long-project-directory-name-{i}/src"))
            .collect();
        directories.push("/".to_string());
        let wide = serde_json::json!([{
            "type": "addDirectories",
            "directories": directories,
            "destination": "userSettings",
        }]);
        let rules: Vec<Value> = (0..12)
            .map(|i| serde_json::json!({"toolName": "Bash", "ruleContent": format!("make target-{i}:*")}))
            .collect();
        let many_rules = serde_json::json!([{
            "type": "addRules", "behavior": "allow", "rules": rules, "destination": "projectSettings",
        }]);
        let bidi = serde_json::json!([{
            "type": "addDirectories",
            "directories": ["/work/proj\u{202e}cod/"],
            "destination": "session",
        }]);
        let comma = serde_json::json!([{
            "type": "addDirectories",
            "directories": ["/work/a, /"],
            "destination": "session",
        }]);
        for (name, suggestions) in [
            ("many directories", &wide),
            ("many rules", &many_rules),
            ("a bidi character", &bidi),
            ("a comma", &comma),
        ] {
            let update = claude_always_update(Some(suggestions)).unwrap();
            assert_eq!(
                claude_update_scope(&update),
                None,
                "{name}: no scope the snapshot cannot show whole"
            );
            let (question, always) = through_event(suggestions);
            assert!(
                always.keyboard_only && always.scope.is_none() && !always.answerable(),
                "{name}: keyboard-only, nothing to confirm: {always:?}"
            );
            assert_eq!(
                question.validate(&[answer(0, &[always.index], None)], true),
                Err(AnswerRefusal::KeyboardOnly),
                "{name}: no client can send it"
            );
            assert_eq!(
                claude_decision(
                    &question,
                    None,
                    Some(suggestions),
                    &forged(&question, &always)
                ),
                None,
                "{name}: no decision grants a target the confirmation did not show"
            );
        }

        // Short enough to show whole: the scope the card shows after
        // sanitizing is byte-for-byte the one the decision is checked against.
        let short = serde_json::json!([{
            "type": "addDirectories",
            "directories": ["/work/proj", "/work/lib"],
            "destination": "localSettings",
        }]);
        let (question, always) = through_event(&short);
        let shown = always.scope.clone().expect("a scope to confirm");
        assert_eq!(
            Some(shown),
            claude_update_scope(&claude_always_update(Some(&short)).unwrap())
        );
        let reply = QuestionReply::answered(
            &question.id,
            question
                .validate(&[answer(0, &[always.index], None)], true)
                .unwrap(),
        );
        assert!(claude_decision(&question, None, Some(&short), &reply).is_some());
    }

    /// Scenario: Every shape an answer can be wrong in is refused with the
    /// reason a client shows — a missing or doubled question, an option that is
    /// not there, two options on a single-select question, text where no
    /// free-text option is chosen and none where one is, a keyboard-only
    /// option, an unconfirmed always-allow and an unsupported channel — and a
    /// right answer resolves to the chosen labels and roles.
    #[spec("question/answer/005")]
    #[test]
    fn question_answer_005_validation_and_the_refusal_wire() {
        let q = form();
        let invalid = |r: Result<Vec<ResolvedAnswer>, AnswerRefusal>| {
            matches!(r, Err(AnswerRefusal::InvalidAnswer { .. }))
        };
        assert!(
            invalid(q.validate(&[answer(0, &[1], None)], false)),
            "question 1 unanswered"
        );
        assert!(invalid(q.validate(
            &[
                answer(0, &[1], None),
                answer(0, &[2], None),
                answer(1, &[1], None)
            ],
            false
        )));
        assert!(invalid(q.validate(
            &[answer(0, &[9], None), answer(1, &[1], None)],
            false
        )));
        assert!(invalid(q.validate(
            &[answer(0, &[1, 2], None), answer(1, &[1], None)],
            false
        )));
        assert!(invalid(q.validate(
            &[answer(0, &[1], Some("x")), answer(1, &[1], None)],
            false
        )));
        assert!(invalid(q.validate(
            &[answer(0, &[3], None), answer(1, &[1], None)],
            false
        )));
        assert!(invalid(q.validate(
            &[answer(0, &[1], None), answer(1, &[], None)],
            false
        )));
        assert!(invalid(q.validate(&[answer(2, &[1], None)], false)));
        assert_eq!(
            q.validate(&[answer(0, &[4], None), answer(1, &[1], None)], false),
            Err(AnswerRefusal::KeyboardOnly)
        );
        let resolved = q
            .validate(
                &[
                    answer(1, &[2, 1], None),
                    answer(0, &[3], Some("  A hamster  ")),
                ],
                false,
            )
            .expect("a whole, valid form");
        assert_eq!(resolved[0].text.as_deref(), Some("A hamster"));
        assert_eq!(resolved[0].roles, vec![OptionRole::FreeText]);
        assert_eq!(resolved[1].labels, vec!["Large", "Small"]);

        let p = permission(AnswerChannel::Held);
        assert_eq!(
            p.validate(&[answer(0, &[2], None)], false),
            Err(AnswerRefusal::AlwaysNotConfirmed)
        );
        assert!(p.validate(&[answer(0, &[2], None)], true).is_ok());
        assert_eq!(
            permission(AnswerChannel::Unsupported).validate(&[answer(0, &[1], None)], false),
            Err(AnswerRefusal::Unsupported)
        );

        // The wire: the request and every refusal round-trip, and an older
        // daemon's response — no `answer_refusal` — still decodes.
        let request = crate::daemon_protocol::AttachRequest::AnswerQuestion {
            agent_id: "7".into(),
            question_id: "q-1".into(),
            answers: vec![answer(0, &[1], None)],
            confirmed_always: true,
            revision: Some(41),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["op"], "answer-question");
        assert_eq!(json["revision"], 41);
        let back: crate::daemon_protocol::AttachRequest = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            crate::daemon_protocol::AttachRequest::AnswerQuestion {
                confirmed_always: true,
                revision: Some(41),
                ..
            }
        ));
        let without_confirmation: crate::daemon_protocol::AttachRequest =
            serde_json::from_value(serde_json::json!({
                "op": "answer-question", "agent_id": "7", "question_id": "q", "answers": []
            }))
            .unwrap();
        assert!(matches!(
            without_confirmation,
            crate::daemon_protocol::AttachRequest::AnswerQuestion {
                confirmed_always: false,
                revision: None,
                ..
            }
        ));
        // An older client's request omits the revision, and a request without
        // one writes no `revision` key.
        let unrevised = crate::daemon_protocol::AttachRequest::AnswerQuestion {
            agent_id: "7".into(),
            question_id: "q-1".into(),
            answers: Vec::new(),
            confirmed_always: false,
            revision: None,
        };
        assert!(
            serde_json::to_value(&unrevised)
                .unwrap()
                .get("revision")
                .is_none()
        );
        for refusal in [
            AnswerRefusal::AgentNotFound,
            AnswerRefusal::NoPendingQuestion,
            AnswerRefusal::Stale {
                current_id: Some("q-2".into()),
            },
            AnswerRefusal::InvalidAnswer { detail: "d".into() },
            AnswerRefusal::KeyboardOnly,
            AnswerRefusal::AlwaysNotConfirmed,
            AnswerRefusal::Unsupported,
            AnswerRefusal::ChannelGone,
            AnswerRefusal::WriteFailed { detail: "w".into() },
            AnswerRefusal::KeyboardStarted,
        ] {
            let resp = crate::daemon_protocol::AttachResponse {
                ok: false,
                error: Some(refusal.to_string()),
                answer_refusal: Some(refusal.clone()),
                ..Default::default()
            };
            let back: crate::daemon_protocol::AttachResponse =
                serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
            assert_eq!(back.answer_refusal, Some(refusal));
        }
        let older: crate::daemon_protocol::AttachResponse =
            serde_json::from_value(serde_json::json!({"ok": false, "error": "malformed request"}))
                .unwrap();
        assert_eq!(older.answer_refusal, None);
    }
}
