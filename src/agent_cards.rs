//! PRD #2374: agent cards.
//!
//! An agent publishes a **report** about itself from its pane
//! (`dot-agent-deck card publish`): a status, a one-line summary, fields,
//! questions for the user and actions it suggests. The daemon keeps the latest
//! report per agent generation (pane + registry agent id) and computes an
//! **entry** for every live agent — the card the desktop renders and `card
//! list` prints — combining the report with what the deck itself knows: the
//! agent's status, whether it needs the user, its group (dispatcher or
//! orchestration), its pull request, and the actions the user can take, each
//! with the exact prompt text it sends and whether it is enabled.
//!
//! Every client renders what this module computes and computes nothing about
//! status, needs-you, enablement or order itself (CLAUDE.md rule 18).
//!
//! Report text is another agent's text: it is bounded, labelled
//! `source: "agent"`, and never executed. No action runs a command; a prompt
//! action posts its exact text to the inbox of the agent whose card it is on,
//! bound to that agent generation, and a close action uses the deck's own stop.
//!
//! Publishing never wakes anyone: it touches neither the inbox nor the
//! work-done update budget (`crate::report_recovery::MAX_REPORT_UPDATES`). Its
//! only side effects are the stored report and the [`EventType::AgentCard`]
//! doorbell, which tells subscribers to re-read the list.
//!
//! [`EventType::AgentCard`]: crate::event::EventType::AgentCard

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agent_pty::{AgentPtyRegistry, AgentRecord, TabMembership};
use crate::card_catalog::{
    self, CatalogAction, FieldType, MAX_PROMPT_CHARS, ResolvedCatalog, has_disallowed_control,
    is_reserved_action_name, is_reserved_field_name, is_valid_name,
};
use crate::event::BroadcastMsg;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// The largest report, serialized, and the largest `card publish` input.
pub const MAX_REPORT_BYTES: usize = 64 * 1024;
pub const MAX_SUMMARY_CHARS: usize = 300;
pub const MAX_FIELDS: usize = 32;
pub const MAX_QUESTIONS: usize = 8;
pub const MAX_QUESTION_TEXT_CHARS: usize = 500;
pub const MAX_OPTIONS: usize = 8;
pub const MAX_REPORT_LABEL_CHARS: usize = 60;
pub const MAX_ACTIONS: usize = 16;
pub const MAX_TEXT_VALUE_CHARS: usize = 300;
pub const MAX_LONG_TEXT_CHARS: usize = 4000;
pub const MAX_URL_CHARS: usize = 2000;
pub const MAX_LIST_ITEMS: usize = 20;
/// The longest `reply` text a user can send through a card.
pub const MAX_REPLY_TEXT_BYTES: usize = 16 * 1024;
/// How long a `request_id` is remembered, so a repeated invocation answers
/// with its first result and sends nothing again.
pub const REQUEST_ID_MEMORY: Duration = Duration::from_secs(10 * 60);
/// The most request ids remembered at once.
const MAX_REMEMBERED_REQUESTS: usize = 1024;
/// The most bytes the remembered request ids and their answers may hold
/// together, besides their count (audit F4).
const MAX_REMEMBERED_BYTES: usize = 1024 * 1024;
/// The longest `request_id` a client may name: `[A-Za-z0-9._:-]`, 1 to 128
/// bytes. A longer or odd one is refused before anything is remembered.
pub const MAX_REQUEST_ID_BYTES: usize = 128;
/// The longest pane id, agent id, action id, action token or instance id an
/// `invoke-card-action` may carry.
pub const MAX_REQUEST_FIELD_BYTES: usize = 256;
/// The longest refusal sentence a result carries.
const MAX_RESULT_MESSAGE_CHARS: usize = 1000;
/// How many recent deliveries an entry keeps, newest first (product decision
/// (a): a delivery's result outlives the report that offered the action).
pub const MAX_RECENT_DELIVERIES: usize = 16;
/// The most text (labels and the exact texts sent) an entry's recent
/// deliveries may hold together; the oldest go first beyond it.
pub const MAX_RECENT_DELIVERY_BYTES: usize = 64 * 1024;
/// How many times a refresh gathers again when a publish or a delivery
/// changed the store while it was gathering (review B4).
const REFRESH_ATTEMPTS: usize = 3;
/// At most this many doorbells per entry per second (4 per second).
const DOORBELL_MIN_GAP: Duration = Duration::from_millis(250);
/// How often the daemon's watcher recomputes the entries while anyone is
/// subscribed.
pub const WATCH_INTERVAL: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Stable codes
// ---------------------------------------------------------------------------

pub const CODE_REPORT_INVALID: &str = "report_invalid";
pub const CODE_REPORT_TOO_LARGE: &str = "report_too_large";
pub const CODE_UNKNOWN_ENTRY: &str = "unknown_entry";
pub const CODE_STALE_ENTRY: &str = "stale_entry";
pub const CODE_UNKNOWN_ACTION: &str = "unknown_action";
pub const CODE_ACTION_DISABLED: &str = "action_disabled";
pub const CODE_CONFIRM_REQUIRED: &str = "confirm_required";
pub const CODE_TEXT_REQUIRED: &str = "text_required";
pub const CODE_CLIENT_SIDE_ACTION: &str = "client_side_action";
pub const CODE_DELIVERY_REFUSED: &str = "delivery_refused";
pub const CODE_CLOSE_FAILED: &str = "close_failed";
pub const CODE_NOT_ATTESTED: &str = "not_attested";
/// The action's definition changed after the card was read (its prompt, its
/// label, whether it asks for confirmation, or the question it answers), or
/// the request named no definition token: nothing was sent. List the cards
/// again and review the action as it is now (review B2, audit F1).
pub const CODE_STALE_ACTION: &str = "stale_action";
/// The request names a daemon process other than the one answering: the
/// deck's daemon was replaced after the card was read, and its agent ids may
/// name unrelated agents now (review B3).
pub const CODE_DAEMON_REPLACED: &str = "daemon_replaced";
/// The `request_id` or another identifier in the request is missing, too
/// long or malformed (audit F4).
pub const CODE_REQUEST_INVALID: &str = "request_invalid";
/// The `request_id` was already used for a different request (audit F6).
pub const CODE_REQUEST_REUSED: &str = "request_reused";
/// The `disabled_reason` of a prompt action whose agent is not running.
pub const REASON_AGENT_STOPPED: &str = "agent_stopped";

/// Metadata keys of the [`crate::event::EventType::AgentCard`] doorbell.
pub const AGENT_CARD_KEY_METADATA_KEY: &str = "agent_card_key";
pub const AGENT_CARD_REVISION_METADATA_KEY: &str = "agent_card_revision";
pub const AGENT_CARD_REMOVED_METADATA_KEY: &str = "agent_card_removed";

/// The inbox `reason` a card action's message is posted under. A
/// daemon-internal string, deliberately not a [`crate::inbox_hub::PostOrigin`]
/// variant: that enum is on the wire, and a new variant would fail an older
/// client's decode.
pub const CARD_ACTION_POST_REASON: &str = "card_action";

// ---------------------------------------------------------------------------
// The report an agent sends
// ---------------------------------------------------------------------------

/// The status an agent reports for itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    #[default]
    Working,
    Waiting,
    Done,
    Failed,
}

impl ReportStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportStatus::Working => "working",
            ReportStatus::Waiting => "waiting",
            ReportStatus::Done => "done",
            ReportStatus::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<ReportStatus> {
        [
            ReportStatus::Working,
            ReportStatus::Waiting,
            ReportStatus::Done,
            ReportStatus::Failed,
        ]
        .into_iter()
        .find(|s| s.as_str() == raw)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportField {
    pub name: String,
    pub value: serde_json::Value,
    /// Custom fields only; ignored for a catalog name.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub field_type: Option<String>,
    /// Custom fields only; ignored for a catalog name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportQuestion {
    pub id: String,
    pub text: String,
    pub options: Vec<ReportOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportAction {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<bool>,
}

/// What an agent publishes. Every publish replaces the whole report; the
/// empty report `{}` clears it. Unknown keys are refused, so a typo is not
/// silently dropped.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ReportStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<ReportField>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<ReportQuestion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ReportAction>,
}

/// Why a report was refused: a stable code and one sentence naming the
/// offending field, question or action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportError {
    pub code: &'static str,
    pub message: String,
}

impl ReportError {
    fn invalid(message: impl Into<String>) -> Self {
        ReportError {
            code: CODE_REPORT_INVALID,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}]", self.message, self.code)
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

fn check_string(
    value: &str,
    what: &str,
    max: usize,
    multiline: bool,
    allow_empty: bool,
) -> Result<(), ReportError> {
    if !allow_empty && value.trim().is_empty() {
        return Err(ReportError::invalid(format!("{what} is empty")));
    }
    if value.chars().count() > max {
        return Err(ReportError::invalid(format!(
            "{what} is longer than {max} characters"
        )));
    }
    if has_disallowed_control(value, multiline) {
        return Err(ReportError::invalid(format!(
            "{what} contains a control character{}",
            if multiline { "" } else { " or a line break" }
        )));
    }
    Ok(())
}

fn quoted(name: &str) -> String {
    format!("{:?}", name.chars().take(48).collect::<String>())
}

/// Check one field value against its type.
pub fn check_value(
    field_type: FieldType,
    value: &serde_json::Value,
    what: &str,
) -> Result<(), ReportError> {
    let expect = |kind: &str| {
        ReportError::invalid(format!(
            "{what} must be {kind} (its type is {})",
            field_type.as_str()
        ))
    };
    match field_type {
        FieldType::Text => {
            let s = value.as_str().ok_or_else(|| expect("a string"))?;
            check_string(s, what, MAX_TEXT_VALUE_CHARS, false, true)
        }
        FieldType::LongText => {
            let s = value.as_str().ok_or_else(|| expect("a string"))?;
            check_string(s, what, MAX_LONG_TEXT_CHARS, true, true)
        }
        FieldType::Number => match value.as_f64() {
            Some(n) if n.is_finite() && value.is_number() => Ok(()),
            _ => Err(expect("a number")),
        },
        FieldType::Boolean => value
            .as_bool()
            .map(|_| ())
            .ok_or_else(|| expect("true or false")),
        FieldType::Url => {
            let s = value.as_str().ok_or_else(|| expect("a string"))?;
            check_string(s, what, MAX_URL_CHARS, false, false)?;
            if !(s.starts_with("https://") || s.starts_with("http://"))
                || s.chars().any(char::is_whitespace)
            {
                return Err(ReportError::invalid(format!(
                    "{what} must be an http:// or https:// URL"
                )));
            }
            Ok(())
        }
        FieldType::List => {
            let items = value
                .as_array()
                .ok_or_else(|| expect("a list of strings"))?;
            if items.len() > MAX_LIST_ITEMS {
                return Err(ReportError::invalid(format!(
                    "{what} has more than {MAX_LIST_ITEMS} items"
                )));
            }
            for item in items {
                let s = item.as_str().ok_or_else(|| expect("a list of strings"))?;
                check_string(s, what, MAX_TEXT_VALUE_CHARS, false, true)?;
            }
            Ok(())
        }
        FieldType::PullRequest | FieldType::Issue => {
            let s = value.as_str().ok_or_else(|| expect("a string"))?;
            check_string(s, what, MAX_TEXT_VALUE_CHARS, false, false)?;
            if is_reference(s) {
                Ok(())
            } else {
                Err(ReportError::invalid(format!(
                    "{what} must be #123, owner/repo#123 or an https:// URL"
                )))
            }
        }
    }
}

/// `#123`, `owner/repo#123`, or an `https://` URL.
fn is_reference(s: &str) -> bool {
    if s.starts_with("https://") {
        return !s.chars().any(char::is_whitespace);
    }
    let Some((repo, number)) = s.rsplit_once('#') else {
        return false;
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if repo.is_empty() {
        return true;
    }
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let ok = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    ok(owner) && ok(name)
}

impl AgentReport {
    /// Parse a JSON report, refusing unknown keys.
    pub fn parse_json(raw: &str) -> Result<AgentReport, ReportError> {
        if raw.len() > MAX_REPORT_BYTES {
            return Err(too_large());
        }
        serde_json::from_str(raw).map_err(|e| {
            ReportError::invalid(format!(
                "the report is not valid: {}",
                first_line(&e.to_string())
            ))
        })
    }

    /// Parse a TOML report (the same keys as JSON), refusing unknown keys.
    pub fn parse_toml(raw: &str) -> Result<AgentReport, ReportError> {
        if raw.len() > MAX_REPORT_BYTES {
            return Err(too_large());
        }
        toml::from_str(raw).map_err(|e| {
            ReportError::invalid(format!(
                "the report is not valid: {}",
                first_line(&e.to_string())
            ))
        })
    }

    pub fn status_or_default(&self) -> ReportStatus {
        self.status.unwrap_or_default()
    }

    /// Check the report against the schema and `catalog`. Refuses the whole
    /// report on the first problem, naming it, rather than dropping parts: a
    /// half-applied card misleads the user.
    pub fn validate(&self, catalog: &ResolvedCatalog) -> Result<(), ReportError> {
        self.validate_with(Some(catalog))
    }

    /// The checks that do not depend on any catalog: sizes, counts, name
    /// syntax, reserved and repeated names, questions, and that no action
    /// asks for confirmation. What `card publish` checks before sending; the
    /// daemon decides the rest — which names are catalog entries, the types
    /// their values must have, and what a custom field or action needs —
    /// against the catalog it resolves cards with (product decision (b),
    /// CLAUDE.md rule 18).
    pub fn validate_schema(&self) -> Result<(), ReportError> {
        self.validate_with(None)
    }

    fn validate_with(&self, catalog: Option<&ResolvedCatalog>) -> Result<(), ReportError> {
        match serde_json::to_vec(self) {
            Ok(bytes) if bytes.len() > MAX_REPORT_BYTES => return Err(too_large()),
            Ok(_) => {}
            Err(e) => return Err(ReportError::invalid(e.to_string())),
        }
        if let Some(summary) = &self.summary {
            check_string(summary, "summary", MAX_SUMMARY_CHARS, false, true)?;
        }
        if self.fields.len() > MAX_FIELDS {
            return Err(ReportError::invalid(format!(
                "the report has {} fields; at most {MAX_FIELDS} are allowed",
                self.fields.len()
            )));
        }
        let mut seen = HashSet::new();
        for field in &self.fields {
            let what = format!("field {}", quoted(&field.name));
            if !is_valid_name(&field.name) {
                return Err(ReportError::invalid(format!(
                    "{what} has a bad name; use 1 to 40 lowercase letters, digits and '-', starting with a letter or digit"
                )));
            }
            if is_reserved_field_name(&field.name) {
                return Err(ReportError::invalid(format!("{what} uses a reserved name")));
            }
            if !seen.insert(field.name.as_str()) {
                return Err(ReportError::invalid(format!("{what} is listed twice")));
            }
            let Some(catalog) = catalog else {
                continue;
            };
            let field_type = match catalog.field(&field.name) {
                Some(entry) => entry.field_type,
                None => {
                    if let Some(label) = &field.label {
                        check_string(
                            label,
                            &format!("{what}'s label"),
                            MAX_REPORT_LABEL_CHARS,
                            false,
                            false,
                        )?;
                    }
                    match field.field_type.as_deref() {
                        None => FieldType::Text,
                        Some(raw) => FieldType::parse(raw).ok_or_else(|| {
                            ReportError::invalid(format!(
                                "{what} has an unknown type {}; expected one of text, long_text, number, boolean, url, list, pull_request, issue",
                                quoted(raw)
                            ))
                        })?,
                    }
                }
            };
            check_value(field_type, &field.value, &what)?;
        }
        if self.questions.len() > MAX_QUESTIONS {
            return Err(ReportError::invalid(format!(
                "the report has {} questions; at most {MAX_QUESTIONS} are allowed",
                self.questions.len()
            )));
        }
        let mut seen = HashSet::new();
        for question in &self.questions {
            let what = format!("question {}", quoted(&question.id));
            if !is_valid_name(&question.id) {
                return Err(ReportError::invalid(format!(
                    "{what} has a bad id; use 1 to 40 lowercase letters, digits and '-'"
                )));
            }
            if !seen.insert(question.id.as_str()) {
                return Err(ReportError::invalid(format!("{what} is listed twice")));
            }
            check_string(
                &question.text,
                &format!("{what}'s text"),
                MAX_QUESTION_TEXT_CHARS,
                true,
                false,
            )?;
            if question.options.is_empty() || question.options.len() > MAX_OPTIONS {
                return Err(ReportError::invalid(format!(
                    "{what} needs 1 to {MAX_OPTIONS} options"
                )));
            }
            let mut ids = HashSet::new();
            for option in &question.options {
                let what = format!("{what}'s option {}", quoted(&option.id));
                if !is_valid_name(&option.id) {
                    return Err(ReportError::invalid(format!("{what} has a bad id")));
                }
                if !ids.insert(option.id.as_str()) {
                    return Err(ReportError::invalid(format!("{what} is listed twice")));
                }
                check_string(
                    &option.label,
                    &format!("{what}'s label"),
                    MAX_REPORT_LABEL_CHARS,
                    false,
                    false,
                )?;
                if let Some(prompt) = &option.prompt {
                    check_string(
                        prompt,
                        &format!("{what}'s prompt"),
                        MAX_PROMPT_CHARS,
                        true,
                        false,
                    )?;
                }
            }
        }
        if self.actions.len() > MAX_ACTIONS {
            return Err(ReportError::invalid(format!(
                "the report has {} actions; at most {MAX_ACTIONS} are allowed",
                self.actions.len()
            )));
        }
        let mut seen = HashSet::new();
        for action in &self.actions {
            let what = format!("action {}", quoted(&action.name));
            if !is_valid_name(&action.name) {
                return Err(ReportError::invalid(format!(
                    "{what} has a bad name; use 1 to 40 lowercase letters, digits and '-'"
                )));
            }
            if is_reserved_action_name(&action.name) {
                return Err(ReportError::invalid(format!("{what} uses a reserved name")));
            }
            if !seen.insert(action.name.as_str()) {
                return Err(ReportError::invalid(format!("{what} is listed twice")));
            }
            let Some(catalog) = catalog else {
                if action.confirm == Some(true) {
                    return Err(ReportError::invalid(format!(
                        "{what} cannot ask for confirmation; only the catalog decides that"
                    )));
                }
                continue;
            };
            if catalog.action(&action.name).is_some() {
                if action.label.is_some() || action.prompt.is_some() || action.confirm.is_some() {
                    return Err(ReportError::invalid(format!(
                        "{what} is a catalog action, so the catalog sets its label, prompt and confirm; leave them out"
                    )));
                }
                continue;
            }
            match &action.label {
                None => {
                    return Err(ReportError::invalid(format!(
                        "{what} is not in the catalog, so it needs a label"
                    )));
                }
                Some(label) => check_string(
                    label,
                    &format!("{what}'s label"),
                    MAX_REPORT_LABEL_CHARS,
                    false,
                    false,
                )?,
            }
            match &action.prompt {
                None => {
                    return Err(ReportError::invalid(format!(
                        "{what} is not in the catalog, so it needs a prompt"
                    )));
                }
                Some(prompt) => check_string(
                    prompt,
                    &format!("{what}'s prompt"),
                    MAX_PROMPT_CHARS,
                    true,
                    false,
                )?,
            }
            if action.confirm == Some(true) {
                return Err(ReportError::invalid(format!(
                    "{what} cannot ask for confirmation; only catalog actions can"
                )));
            }
        }
        Ok(())
    }
}

fn too_large() -> ReportError {
    ReportError {
        code: CODE_REPORT_TOO_LARGE,
        message: format!("the report is larger than {MAX_REPORT_BYTES} bytes"),
    }
}

// ---------------------------------------------------------------------------
// What clients receive
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardGroup {
    pub id: String,
    /// `dispatcher`, `orchestration` or `standalone`.
    pub kind: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardDispatcherRelation {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardOrchestrationRelation {
    pub id: String,
    pub label: String,
    pub role: String,
    pub is_orchestrator: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardRelations {
    #[serde(default)]
    pub dispatcher: Option<CardDispatcherRelation>,
    #[serde(default)]
    pub orchestration: Option<CardOrchestrationRelation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardReport {
    /// Always `agent`: everything under it is the agent's own word.
    pub source: String,
    pub status: String,
    #[serde(default)]
    pub summary: Option<String>,
    pub published_at: DateTime<Utc>,
    /// How many times this agent generation has published.
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CardField {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub icon: Option<String>,
    pub placement: String,
    pub custom: bool,
    pub source: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardQuestionOption {
    pub id: String,
    pub label: String,
    pub action_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardAnswered {
    pub option_id: String,
    pub action_id: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardQuestion {
    pub id: String,
    pub text: String,
    pub source: String,
    pub options: Vec<CardQuestionOption>,
    #[serde(default)]
    pub answered: Option<CardAnswered>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardDelivery {
    pub message_id: String,
    /// `queued`, `delivered`, `uncertain` or `not_delivered`.
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardAction {
    pub id: String,
    pub label: String,
    /// `prompt`, `close`, `close_keep_worktree` or `open`.
    pub kind: String,
    /// The exact text a prompt action sends; `null` for `reply` (the user's
    /// text is sent as is) and for the deck's other actions.
    #[serde(default)]
    pub prompt: Option<String>,
    pub text_input: bool,
    pub confirm: bool,
    #[serde(default)]
    pub confirm_detail: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    pub placement: String,
    pub custom: bool,
    /// `catalog`, `agent` or `deck`.
    pub source: String,
    pub enabled: bool,
    #[serde(default)]
    pub disabled_reason: Option<String>,
    /// The latest delivery of THIS definition of the action (its `token`), so
    /// an action id an agent reused for new text does not show the old
    /// text's delivery (review S1). Every delivery, of this definition or an
    /// earlier one, is in the entry's `recent_deliveries`.
    #[serde(default)]
    pub last_delivery: Option<CardDelivery>,
    /// The daemon's name for this definition of the action: a digest of the
    /// card, the action id, its kind, label, exact prompt, whether it asks
    /// for confirmation and, for a question's option, the question and the
    /// option. An invocation names it, and the daemon refuses one whose
    /// action changed since (`stale_action`), so a press can only send the
    /// text the user was shown (review B2, audit F1). Unchanged by anything
    /// else: a status, a delivery, the close's pending note.
    #[serde(default)]
    pub token: String,
}

/// One action the user invoked on a card, kept on the entry after a later
/// report stops offering the action (product decision (a)): what was sent,
/// exactly, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardDeliveryRecord {
    pub action_id: String,
    /// The label the action had when it was invoked.
    pub label: String,
    /// The exact text sent: the action's prompt, or the user's reply.
    pub text: String,
    /// The action definition's `token` when it was invoked.
    pub token: String,
    /// `sent`.
    pub outcome: String,
    pub message_id: String,
    /// `queued`, `delivered`, `uncertain` or `not_delivered`.
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardFacts {
    /// Always `deck`: everything under it the deck checked itself.
    pub source: String,
    #[serde(default)]
    pub pull_request: Option<crate::pull_request_info::PullRequestInfo>,
}

/// One agent's card, as the daemon computed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCardEntry {
    pub key: String,
    pub instance_id: String,
    pub pane_id: String,
    pub agent_id: String,
    #[serde(default)]
    pub agent_type: Option<crate::event::AgentType>,
    pub name: String,
    pub status: String,
    pub needs_you: bool,
    pub needs_you_reasons: Vec<String>,
    pub group: CardGroup,
    #[serde(default)]
    pub relations: CardRelations,
    #[serde(default)]
    pub report: Option<CardReport>,
    #[serde(default)]
    pub fields: Vec<CardField>,
    #[serde(default)]
    pub questions: Vec<CardQuestion>,
    #[serde(default)]
    pub actions: Vec<CardAction>,
    pub primary_action_id: String,
    pub facts: CardFacts,
    /// The actions invoked on this card, newest first, at most
    /// [`MAX_RECENT_DELIVERIES`] and [`MAX_RECENT_DELIVERY_BYTES`] of text.
    #[serde(default)]
    pub recent_deliveries: Vec<CardDeliveryRecord>,
    pub revision: u64,
    pub updated_at: DateTime<Utc>,
}

impl AgentCardEntry {
    pub fn action(&self, id: &str) -> Option<&CardAction> {
        self.actions.iter().find(|a| a.id == id)
    }
}

/// `list-agent-cards`'s answer: every live entry in display order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCardList {
    pub instance_id: String,
    pub revision: u64,
    pub entries: Vec<AgentCardEntry>,
    #[serde(default)]
    pub catalog_errors: Vec<String>,
}

/// `invoke-card-action`'s answer. A refusal is `outcome: "refused"` with a
/// stable `code`, so a client can tell it from a transport error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardActionResult {
    pub request_id: String,
    /// `sent`, `closed` or `refused`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_revision: Option<u64>,
}

impl CardActionResult {
    pub fn refused(request_id: &str, code: &str, message: impl Into<String>) -> Self {
        let message: String = message.into();
        CardActionResult {
            request_id: request_id.to_string(),
            outcome: "refused".to_string(),
            message_id: None,
            kept_worktree: None,
            code: Some(code.to_string()),
            message: Some(message.chars().take(MAX_RESULT_MESSAGE_CHARS).collect()),
            entry_revision: None,
        }
    }

    pub fn is_refused(&self) -> bool {
        self.outcome == "refused"
    }
}

/// One action of one card, named the way an invocation must name it: the
/// daemon process that listed the card, the card's pane and agent
/// generation, the action and the `token` of the definition the user was
/// shown. Each part comes from `card list` (`instance_id`, `pane_id`,
/// `agent_id`, the action's `id` and `token`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardActionTarget {
    pub instance_id: String,
    pub pane_id: String,
    pub agent_id: String,
    pub action_id: String,
    pub action_token: String,
}

/// The key of an agent generation's card.
pub fn entry_key(pane_id: &str, agent_id: &str) -> String {
    format!("{pane_id}/{agent_id}")
}

/// Split `<pane>/<agent>` at its last `/`.
pub fn split_key(key: &str) -> Option<(&str, &str)> {
    let (pane, agent) = key.rsplit_once('/')?;
    (!pane.is_empty() && !agent.is_empty()).then_some((pane, agent))
}

// ---------------------------------------------------------------------------
// Projection: facts in, entries out (pure)
// ---------------------------------------------------------------------------

/// What the daemon records for one agent generation, besides its report.
#[derive(Debug, Clone, Default)]
struct Slot {
    report: Option<StoredReport>,
    /// The actions invoked on this card, newest first, bounded by count and
    /// by bytes (audit F5): every invocation adds one and the oldest go.
    deliveries: std::collections::VecDeque<CardDeliveryRecord>,
    /// Questions answered since the last publish, by question id.
    answered: HashMap<String, CardAnswered>,
    /// The store's epoch when this slot was last published to or delivered
    /// on. A refresh whose snapshot of the registry was taken before it does
    /// not prune it (review B4).
    epoch: u64,
}

impl Slot {
    /// Record a delivery, newest first, dropping the oldest beyond the
    /// count and byte bounds.
    fn push_delivery(&mut self, record: CardDeliveryRecord) {
        self.deliveries.push_front(record);
        self.deliveries.truncate(MAX_RECENT_DELIVERIES);
        let size = |r: &CardDeliveryRecord| r.text.len() + r.label.len() + r.action_id.len();
        let mut total: usize = self.deliveries.iter().map(size).sum();
        while total > MAX_RECENT_DELIVERY_BYTES && self.deliveries.len() > 1 {
            if let Some(oldest) = self.deliveries.pop_back() {
                total -= size(&oldest);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct StoredReport {
    report: AgentReport,
    published_at: DateTime<Utc>,
    publishes: u64,
}

/// One agent generation as the deck sees it, gathered before projection.
#[derive(Debug, Clone)]
pub struct AgentFacts {
    /// The registry record, with its live session joined in.
    pub record: AgentRecord,
    pub pane_id: String,
    /// The process exited on its own (`crashed`) or has been seen to exit.
    pub stopped: bool,
    /// Messages owed to the agent and not yet acknowledged.
    pub undelivered_messages: usize,
    /// Whether the daemon records a dispatched worktree for the agent, which
    /// is what offers "close, keeping the worktree".
    pub has_worktree: bool,
}

/// A live dispatched unit, as projection needs it.
#[derive(Debug, Clone)]
pub struct UnitFacts {
    pub dispatcher_pane: String,
    pub dispatcher_agent: String,
    /// The generation that is the unit, for a single-agent unit.
    pub single: Option<(String, String)>,
    /// The orchestration instance that is the unit.
    pub orchestration_id: Option<String>,
}

impl UnitFacts {
    fn contains(&self, pane: &str, agent: &str, orchestration_id: Option<&str>) -> bool {
        if let Some((p, a)) = &self.single {
            return p == pane && a == agent;
        }
        matches!((&self.orchestration_id, orchestration_id), (Some(u), Some(o)) if u == o)
    }
}

fn session_status_name(status: &crate::state::SessionStatus) -> &'static str {
    use crate::state::SessionStatus as S;
    match status {
        S::Thinking => "thinking",
        S::Working => "working",
        S::Compacting => "compacting",
        S::WaitingForInput => "waiting_for_input",
        S::Idle => "idle",
        S::Error => "error",
        S::Blocked => "blocked",
        S::Waiting => "waiting",
        S::Unknown => "unknown",
    }
}

fn orchestration_of(record: &AgentRecord) -> Option<(&str, String, &str, bool)> {
    match &record.tab_membership {
        Some(TabMembership::Orchestration {
            name,
            role_name,
            is_start_role,
            display_title,
            orchestration_id: Some(id),
            ..
        }) => Some((
            id.as_str(),
            display_title.clone().unwrap_or_else(|| name.clone()),
            role_name.as_str(),
            *is_start_role,
        )),
        _ => None,
    }
}

/// The name a card shows: the record's display name, else its orchestration
/// role, else its agent type's name, else its pane id.
pub fn agent_name(record: &AgentRecord, pane_id: &str) -> String {
    if let Some(name) = record
        .display_name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
    {
        return name.to_string();
    }
    if let Some(TabMembership::Orchestration { role_name, .. }) = &record.tab_membership
        && !role_name.trim().is_empty()
    {
        return role_name.clone();
    }
    let agent_type = record
        .live
        .as_ref()
        .and_then(|l| l.agent_type.clone())
        .or_else(|| record.agent_type.clone());
    if let Some(t) = agent_type.filter(|t| *t != crate::event::AgentType::None) {
        return crate::hook_binary::agent_display_name(&t).to_string();
    }
    pane_id.to_string()
}

fn spawn_order(agent_id: &str) -> u64 {
    agent_id.parse().unwrap_or(u64::MAX)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        format!("{n} {many}")
    }
}

/// The type a custom field resolves to: the type the report gave it, else
/// `text`; `None` when the report's type is not one the deck knows (possible
/// for a field that was a catalog field when published, whose `type` was
/// ignored then).
fn custom_field_type(field: &ReportField) -> Option<FieldType> {
    match field.field_type.as_deref() {
        None => Some(FieldType::Text),
        Some(raw) => FieldType::parse(raw),
    }
}

/// A custom field's or action's label, when it passes the report rules.
fn custom_label(label: Option<&str>) -> Option<String> {
    let label = label?;
    check_string(label, "label", MAX_REPORT_LABEL_CHARS, false, false).ok()?;
    Some(label.to_string())
}

/// A report action not in the catalog, as a custom action: its label and
/// prompt when both pass the report rules for a custom action, else `None`.
fn custom_definition(action: &ReportAction) -> Option<(String, String)> {
    if action.confirm == Some(true) {
        return None;
    }
    let label = custom_label(action.label.as_deref())?;
    let prompt = action.prompt.as_deref()?;
    check_string(prompt, "prompt", MAX_PROMPT_CHARS, true, false).ok()?;
    Some((label, prompt.to_string()))
}

/// The token of one action definition: what [`CardAction::token`] says.
fn action_token(
    instance_id: &str,
    key: &str,
    action: &CardAction,
    question: Option<(&ReportQuestion, &ReportOption)>,
) -> String {
    use sha2::{Digest as _, Sha256};
    #[derive(Serialize)]
    struct Definition<'a> {
        instance_id: &'a str,
        key: &'a str,
        id: &'a str,
        kind: &'a str,
        label: &'a str,
        prompt: Option<&'a str>,
        confirm: bool,
        text_input: bool,
        question: Option<[&'a str; 4]>,
    }
    let bytes = serde_json::to_vec(&Definition {
        instance_id,
        key,
        id: &action.id,
        kind: &action.kind,
        label: &action.label,
        prompt: action.prompt.as_deref(),
        confirm: action.confirm,
        text_input: action.text_input,
        question: question.map(|(q, o)| {
            [
                q.id.as_str(),
                q.text.as_str(),
                o.id.as_str(),
                o.label.as_str(),
            ]
        }),
    })
    .unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("act-{hex}")
}

/// Compute one entry (revision and `updated_at` left for the store).
fn project_entry(
    facts: &AgentFacts,
    slot: Option<&Slot>,
    catalog: &ResolvedCatalog,
    units: &[UnitFacts],
    names: &HashMap<String, String>,
    instance_id: &str,
) -> AgentCardEntry {
    let record = &facts.record;
    let pane = facts.pane_id.as_str();
    let agent = record.id.as_str();
    let key = entry_key(pane, agent);
    let live = record.live.as_ref();
    let report = slot.and_then(|s| s.report.as_ref());
    let empty_answers = HashMap::new();
    let answered = slot.map_or(&empty_answers, |s| &s.answered);

    // Status: stopped wins over everything.
    let status = if facts.stopped {
        "stopped".to_string()
    } else {
        live.map_or("unknown", |l| session_status_name(&l.status))
            .to_string()
    };

    // Questions.
    let questions: Vec<CardQuestion> = report
        .map(|r| {
            r.report
                .questions
                .iter()
                .map(|q| CardQuestion {
                    id: q.id.clone(),
                    text: q.text.clone(),
                    source: "agent".into(),
                    options: q
                        .options
                        .iter()
                        .map(|o| CardQuestionOption {
                            id: o.id.clone(),
                            label: o.label.clone(),
                            action_id: format!("question/{}/{}", q.id, o.id),
                        })
                        .collect(),
                    answered: answered.get(&q.id).cloned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let unanswered = questions.iter().filter(|q| q.answered.is_none()).count();
    let native_question = live.is_some_and(|l| l.pending_question.is_some());

    // Needs you, in the documented order.
    let mut reasons: Vec<String> = Vec::new();
    if facts.stopped {
        reasons.push("stopped".into());
    }
    if native_question {
        reasons.push("native_question".into());
    }
    if unanswered > 0 {
        reasons.push("question".into());
    }
    if let Some(r) = report {
        match r.report.status_or_default() {
            ReportStatus::Waiting => reasons.push("waiting".into()),
            ReportStatus::Done => reasons.push("done".into()),
            ReportStatus::Failed => reasons.push("failed".into()),
            ReportStatus::Working => {}
        }
    }
    if status == "blocked" {
        reasons.push("blocked".into());
    }

    // Fields: catalog fields in catalog order, then custom ones in report
    // order. The catalog may have changed since the report was published
    // (review B7), so each value is checked again against the type it
    // resolves to NOW, and a value that no longer fits is not shown until
    // the agent publishes again. A field the catalog dropped is shown as the
    // agent's own custom field, with the type and label the report gave.
    let mut fields = Vec::new();
    if let Some(r) = report {
        for entry in &catalog.fields {
            if let Some(f) = r.report.fields.iter().find(|f| f.name == entry.name)
                && check_value(entry.field_type, &f.value, "value").is_ok()
            {
                fields.push(CardField {
                    name: entry.name.clone(),
                    label: entry.label.clone(),
                    description: Some(entry.description.clone()),
                    field_type: entry.field_type.as_str().into(),
                    icon: entry.icon.clone(),
                    placement: entry.placement.as_str().into(),
                    custom: false,
                    source: "agent".into(),
                    value: f.value.clone(),
                });
            }
        }
        for f in &r.report.fields {
            if catalog.field(&f.name).is_some() {
                continue;
            }
            let Some(field_type) = custom_field_type(f) else {
                continue;
            };
            if check_value(field_type, &f.value, "value").is_err() {
                continue;
            }
            fields.push(CardField {
                name: f.name.clone(),
                label: custom_label(f.label.as_deref()).unwrap_or_else(|| f.name.clone()),
                description: None,
                field_type: field_type.as_str().into(),
                icon: None,
                placement: "detail".into(),
                custom: true,
                source: "agent".into(),
                value: f.value.clone(),
            });
        }
    }

    let pull_request = live.and_then(|l| l.pull_request.clone());
    let prompt_enabled = !facts.stopped;
    let prompt_action = |id: String,
                         label: String,
                         prompt: Option<String>,
                         icon: Option<String>,
                         placement: &str,
                         confirm: bool,
                         custom: bool,
                         source: &str,
                         text_input: bool| CardAction {
        last_delivery: None,
        id,
        label,
        kind: "prompt".into(),
        prompt,
        text_input,
        confirm,
        confirm_detail: None,
        icon,
        placement: placement.into(),
        custom,
        source: source.into(),
        enabled: prompt_enabled,
        disabled_reason: (!prompt_enabled).then(|| REASON_AGENT_STOPPED.to_string()),
        token: String::new(),
    };

    let mut actions: Vec<CardAction> = Vec::new();
    // The question and option behind each option action, for its token.
    let mut answers: HashMap<String, (&ReportQuestion, &ReportOption)> = HashMap::new();
    let mut primary_candidates: Vec<String> = Vec::new();
    // 1. Question options.
    if let Some(r) = report {
        for q in &r.report.questions {
            for o in &q.options {
                let id = format!("question/{}/{}", q.id, o.id);
                let prompt = o.prompt.clone().unwrap_or_else(|| {
                    format!("Answer to your question \"{}\": {}", q.text, o.label)
                });
                if answered.get(&q.id).is_none() {
                    primary_candidates.push(id.clone());
                }
                answers.insert(id.clone(), (q, o));
                actions.push(prompt_action(
                    id,
                    o.label.clone(),
                    Some(prompt),
                    None,
                    "card",
                    false,
                    false,
                    "agent",
                    false,
                ));
            }
        }
    }
    // 2. Report-suggested catalog actions, in report order.
    let push_catalog = |actions: &mut Vec<CardAction>, a: &CatalogAction| {
        actions.push(prompt_action(
            a.name.clone(),
            a.label.clone(),
            Some(a.prompt.clone()),
            a.icon.clone(),
            a.placement.as_str(),
            a.confirm,
            false,
            "catalog",
            false,
        ));
    };
    let mut custom_suggested = Vec::new();
    if let Some(r) = report {
        for suggested in &r.report.actions {
            match catalog.action(&suggested.name) {
                Some(a) => {
                    primary_candidates.push(a.name.clone());
                    push_catalog(&mut actions, a);
                }
                None => custom_suggested.push(suggested),
            }
        }
    }
    // 3. `always` catalog actions not already listed.
    for a in catalog.actions.iter().filter(|a| a.always) {
        if !actions.iter().any(|x| x.id == a.name) {
            push_catalog(&mut actions, a);
        }
    }
    // 4. Custom report actions. A suggestion the catalog answered when it was
    // published, and no longer does, carries no label or prompt of its own:
    // it is not offered until the agent publishes again, rather than being
    // turned into a custom action with an empty prompt (review B7).
    for suggested in custom_suggested {
        let Some((label, prompt)) = custom_definition(suggested) else {
            continue;
        };
        primary_candidates.push(suggested.name.clone());
        actions.push(prompt_action(
            suggested.name.clone(),
            label,
            Some(prompt),
            None,
            "card",
            false,
            true,
            "agent",
            false,
        ));
    }
    // 5. The deck's own actions.
    actions.push(prompt_action(
        "reply".into(),
        "Reply".into(),
        None,
        Some("message-square".into()),
        "detail",
        false,
        false,
        "deck",
        true,
    ));
    let deck_action = |id: &str, label: &str, kind: &str, icon: &str, confirm_detail| CardAction {
        last_delivery: None,
        id: id.into(),
        label: label.into(),
        kind: kind.into(),
        prompt: None,
        text_input: false,
        confirm: kind != "open",
        confirm_detail,
        icon: Some(icon.into()),
        placement: "detail".into(),
        custom: false,
        source: "deck".into(),
        enabled: true,
        disabled_reason: None,
        token: String::new(),
    };
    actions.push(deck_action("open", "Open", "open", "terminal", None));
    let pending = {
        let mut parts = Vec::new();
        let questions_pending = unanswered + usize::from(native_question);
        if questions_pending > 0 {
            parts.push(plural(
                questions_pending,
                "an unanswered question",
                "unanswered questions",
            ));
        }
        if let Some(pr) = &pull_request
            && matches!(
                pr.state,
                crate::pull_request_info::PullRequestState::Open
                    | crate::pull_request_info::PullRequestState::Draft
            )
        {
            parts.push(format!("open pull request #{}", pr.number));
        }
        if facts.undelivered_messages > 0 {
            parts.push(if facts.undelivered_messages == 1 {
                "1 undelivered message".to_string()
            } else {
                format!("{} undelivered messages", facts.undelivered_messages)
            });
        }
        (!parts.is_empty()).then(|| format!("Still pending: {}.", parts.join("; ")))
    };
    actions.push(deck_action("close", "Close", "close", "x", pending.clone()));
    if facts.has_worktree {
        actions.push(deck_action(
            "close-keep-worktree",
            "Close, keep worktree",
            "close_keep_worktree",
            "archive",
            pending,
        ));
    }

    // Each definition's token, and the latest delivery OF that definition.
    let recent_deliveries: Vec<CardDeliveryRecord> = slot
        .map(|s| s.deliveries.iter().cloned().collect())
        .unwrap_or_default();
    for a in &mut actions {
        a.token = action_token(instance_id, &key, a, answers.get(&a.id).copied());
        a.last_delivery = recent_deliveries
            .iter()
            .find(|d| d.action_id == a.id && d.token == a.token)
            .map(|d| CardDelivery {
                message_id: d.message_id.clone(),
                state: d.state.clone(),
                detail: d.detail.clone(),
                at: d.at,
            });
    }

    let primary_action_id = primary_candidates
        .into_iter()
        .find(|id| actions.iter().any(|a| &a.id == id && a.enabled))
        .unwrap_or_else(|| "open".to_string());

    // Group and relations.
    let orchestration = orchestration_of(record);
    let unit = units
        .iter()
        .find(|u| u.contains(pane, agent, orchestration.as_ref().map(|o| o.0)));
    let name_of = |key: &str, fallback: &str| {
        names
            .get(key)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    };
    let dispatcher_rel = unit.map(|u| {
        let dkey = entry_key(&u.dispatcher_pane, &u.dispatcher_agent);
        CardDispatcherRelation {
            name: name_of(&dkey, "dispatcher"),
            key: dkey,
        }
    });
    let is_dispatcher = units
        .iter()
        .any(|u| u.dispatcher_pane == pane && u.dispatcher_agent == agent);
    let group = if let Some(d) = &dispatcher_rel {
        CardGroup {
            id: format!("dispatcher:{}", d.key),
            kind: "dispatcher".into(),
            label: Some(d.name.clone()),
        }
    } else if is_dispatcher {
        CardGroup {
            id: format!("dispatcher:{key}"),
            kind: "dispatcher".into(),
            label: Some(name_of(&key, "dispatcher")),
        }
    } else if let Some((id, label, _, _)) = &orchestration {
        CardGroup {
            id: (*id).to_string(),
            kind: "orchestration".into(),
            label: Some(label.clone()),
        }
    } else {
        CardGroup {
            id: "standalone".into(),
            kind: "standalone".into(),
            label: None,
        }
    };
    let relations = CardRelations {
        dispatcher: dispatcher_rel,
        orchestration: orchestration.map(|(id, label, role, is_orchestrator)| {
            CardOrchestrationRelation {
                id: id.to_string(),
                label,
                role: role.to_string(),
                is_orchestrator,
            }
        }),
    };

    let agent_type = live
        .and_then(|l| l.agent_type.clone())
        .or_else(|| record.agent_type.clone())
        .filter(|t| *t != crate::event::AgentType::None);

    AgentCardEntry {
        key: key.clone(),
        instance_id: instance_id.to_string(),
        pane_id: pane.to_string(),
        agent_id: agent.to_string(),
        agent_type,
        name: name_of(&key, pane),
        status,
        needs_you: !reasons.is_empty(),
        needs_you_reasons: reasons,
        group,
        relations,
        report: report.map(|r| CardReport {
            source: "agent".into(),
            status: r.report.status_or_default().as_str().into(),
            summary: r.report.summary.clone(),
            published_at: r.published_at,
            revision: r.publishes,
        }),
        fields,
        questions,
        actions,
        primary_action_id,
        facts: CardFacts {
            source: "deck".into(),
            pull_request,
        },
        recent_deliveries,
        revision: 0,
        updated_at: DateTime::<Utc>::UNIX_EPOCH,
    }
}

/// Every entry for `agents`, in display order: groups in order of their
/// first-spawned member, within a group the entries that need the user first,
/// then spawn order.
fn project_all(
    agents: &[AgentFacts],
    slots: &HashMap<String, Slot>,
    catalog: &ResolvedCatalog,
    units: &[UnitFacts],
    instance_id: &str,
) -> Vec<AgentCardEntry> {
    let names: HashMap<String, String> = agents
        .iter()
        .map(|a| {
            (
                entry_key(&a.pane_id, &a.record.id),
                agent_name(&a.record, &a.pane_id),
            )
        })
        .collect();
    let mut entries: Vec<AgentCardEntry> = agents
        .iter()
        .map(|a| {
            let key = entry_key(&a.pane_id, &a.record.id);
            project_entry(a, slots.get(&key), catalog, units, &names, instance_id)
        })
        .collect();
    let mut group_first: HashMap<String, u64> = HashMap::new();
    for e in &entries {
        let order = spawn_order(&e.agent_id);
        group_first
            .entry(e.group.id.clone())
            .and_modify(|o| *o = (*o).min(order))
            .or_insert(order);
    }
    entries.sort_by(|a, b| {
        let ga = group_first[&a.group.id];
        let gb = group_first[&b.group.id];
        ga.cmp(&gb)
            .then_with(|| a.group.id.cmp(&b.group.id))
            .then_with(|| b.needs_you.cmp(&a.needs_you))
            .then_with(|| spawn_order(&a.agent_id).cmp(&spawn_order(&b.agent_id)))
            .then_with(|| a.key.cmp(&b.key))
    });
    entries
}

fn content_hash(entry: &AgentCardEntry) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(entry)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// The daemon's store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Computed {
    hash: u64,
    revision: u64,
    updated_at: DateTime<Utc>,
    last_doorbell: Option<Instant>,
    doorbell_owed: bool,
}

/// What an invocation's `request_id` is remembered as: still running, so a
/// repeat waits for its answer, or answered, so a repeat gets that answer
/// and nothing is done again (review B5, audit F6).
enum Remembered {
    InFlight {
        fingerprint: [u8; 32],
        at: Instant,
        answer: tokio::sync::watch::Receiver<Option<CardActionResult>>,
    },
    Done {
        fingerprint: [u8; 32],
        at: Instant,
        result: CardActionResult,
    },
}

impl Remembered {
    fn at(&self) -> Instant {
        match self {
            Remembered::InFlight { at, .. } | Remembered::Done { at, .. } => *at,
        }
    }

    /// What it holds, besides its key: the answer's own strings.
    fn bytes(&self) -> usize {
        match self {
            Remembered::InFlight { .. } => 0,
            Remembered::Done { result, .. } => {
                result.request_id.len()
                    + result.outcome.len()
                    + result.message_id.as_ref().map_or(0, String::len)
                    + result.kept_worktree.as_ref().map_or(0, String::len)
                    + result.code.as_ref().map_or(0, String::len)
                    + result.message.as_ref().map_or(0, String::len)
            }
        }
    }
}

/// What [`CardStore::claim`] answers for a request id.
enum Claim {
    /// Nobody has it: run the invocation and send its answer here.
    Run(tokio::sync::watch::Sender<Option<CardActionResult>>),
    /// The same request is running: wait for its answer.
    Wait(tokio::sync::watch::Receiver<Option<CardActionResult>>),
    /// Answered already (or refused as a reused id).
    Answered(CardActionResult),
}

#[derive(Default)]
struct StoreInner {
    slots: HashMap<String, Slot>,
    computed: HashMap<String, Computed>,
    counter: u64,
    /// Bumped by every publish and every delivery recorded. A refresh that
    /// read the registry before a bump does not prune by that reading
    /// (review B4).
    epoch: u64,
    requests: HashMap<String, Remembered>,
    catalog: Option<CachedCatalog>,
}

impl StoreInner {
    /// Forget the answers older than [`REQUEST_ID_MEMORY`], and the running
    /// claims whose invocation went away without answering.
    fn forget_expired(&mut self, now: Instant) {
        self.requests.retain(|_, r| match r {
            Remembered::Done { at, .. } => now.duration_since(*at) < REQUEST_ID_MEMORY,
            Remembered::InFlight { at, answer, .. } => {
                now.duration_since(*at) < REQUEST_ID_MEMORY || answer.has_changed().is_ok()
            }
        });
    }

    /// Drop the oldest answers until the remembered ids fit their count and
    /// byte bounds. A running claim is never dropped.
    fn make_room(&mut self, incoming: usize) {
        loop {
            let bytes: usize = self.requests.iter().map(|(k, r)| k.len() + r.bytes()).sum();
            if self.requests.len() < MAX_REMEMBERED_REQUESTS
                && bytes + incoming <= MAX_REMEMBERED_BYTES
            {
                return;
            }
            let oldest = self
                .requests
                .iter()
                .filter(|(_, r)| matches!(r, Remembered::Done { .. }))
                .min_by_key(|(_, r)| r.at())
                .map(|(k, _)| k.clone());
            match oldest {
                Some(key) => {
                    self.requests.remove(&key);
                }
                None => return,
            }
        }
    }

    /// Commit one reading of the deck: prune the slots of generations that
    /// are gone, update delivery states, project every entry, and assign
    /// revisions and doorbells to the ones that changed.
    ///
    /// `read_at` is the store's epoch when the deck was read. A slot
    /// published to (or delivered on) after it is never pruned by this
    /// reading, which may predate its agent (review B4); and when `retry` is
    /// set and the store changed since the reading, nothing is committed and
    /// `None` asks the caller to read the deck again.
    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        agents: &[AgentFacts],
        units: &[UnitFacts],
        states: &HashMap<String, (String, Option<String>)>,
        catalog: &ResolvedCatalog,
        instance_id: &str,
        read_at: u64,
        retry: bool,
        now: DateTime<Utc>,
        instant: Instant,
    ) -> Option<(AgentCardList, Vec<Doorbell>)> {
        if retry && self.epoch != read_at {
            return None;
        }
        let mut doorbells = Vec::new();
        let live: HashSet<String> = agents
            .iter()
            .map(|a| entry_key(&a.pane_id, &a.record.id))
            .collect();
        // A closed or replaced generation's report and delivery state go
        // with it — unless the slot changed after the deck was read.
        self.slots
            .retain(|k, slot| live.contains(k) || slot.epoch > read_at);
        for slot in self.slots.values_mut() {
            for delivery in slot.deliveries.iter_mut() {
                if let Some((state, detail)) = states.get(&delivery.message_id) {
                    delivery.state = state.clone();
                    delivery.detail = detail.clone();
                }
            }
        }
        let mut entries = project_all(agents, &self.slots, catalog, units, instance_id);
        let removed: Vec<String> = self
            .computed
            .keys()
            .filter(|k| !live.contains(*k))
            .cloned()
            .collect();
        for key in removed {
            self.computed.remove(&key);
            self.counter += 1;
            doorbells.push(Doorbell {
                key,
                revision: self.counter,
                removed: true,
            });
        }
        for entry in &mut entries {
            let hash = content_hash(entry);
            let counter = self.counter;
            let computed = self.computed.get(&entry.key).cloned();
            let mut c = match computed {
                Some(c) if c.hash == hash => c,
                Some(mut c) => {
                    c.hash = hash;
                    c.revision = counter + 1;
                    c.updated_at = now;
                    c.doorbell_owed = true;
                    self.counter += 1;
                    c
                }
                None => {
                    self.counter += 1;
                    Computed {
                        hash,
                        revision: counter + 1,
                        updated_at: now,
                        last_doorbell: None,
                        doorbell_owed: true,
                    }
                }
            };
            if c.doorbell_owed
                && c.last_doorbell
                    .is_none_or(|at| instant.duration_since(at) >= DOORBELL_MIN_GAP)
            {
                c.doorbell_owed = false;
                c.last_doorbell = Some(instant);
                doorbells.push(Doorbell {
                    key: entry.key.clone(),
                    revision: c.revision,
                    removed: false,
                });
            }
            entry.revision = c.revision;
            entry.updated_at = c.updated_at;
            self.computed.insert(entry.key.clone(), c);
        }
        Some((
            AgentCardList {
                instance_id: instance_id.to_string(),
                revision: self.counter,
                entries,
                catalog_errors: catalog.errors.clone(),
            },
            doorbells,
        ))
    }
}

struct CachedCatalog {
    stamp: Option<(std::path::PathBuf, Option<std::time::SystemTime>, u64)>,
    catalog: Arc<ResolvedCatalog>,
}

/// The daemon's agent-card state: the reports agents published, the
/// deliveries of card actions, and the revisions of the entries computed
/// from them. In memory only, like the agents it describes.
#[derive(Default)]
pub struct CardStore {
    inner: Mutex<StoreInner>,
    /// One turn at a time for everything that reads the deck and commits
    /// what it read — a refresh — and for everything that changes a slot —
    /// a publish, a prompt delivery. A refresh's reading and its commit are
    /// one turn, so a publish can never land between them (review B4).
    serial: tokio::sync::Mutex<()>,
    broadcast: Mutex<Option<broadcast::Sender<BroadcastMsg>>>,
    worktrees: Mutex<Option<crate::issue_dispatch_run::WorktreeRegistry>>,
}

impl std::fmt::Debug for CardStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CardStore").finish_non_exhaustive()
    }
}

impl CardStore {
    fn lock(&self) -> MutexGuard<'_, StoreInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where doorbells go, and which worktree registry says whether an agent
    /// has a dispatched worktree. Installed once by the daemon.
    pub fn install(
        &self,
        tx: broadcast::Sender<BroadcastMsg>,
        worktrees: crate::issue_dispatch_run::WorktreeRegistry,
    ) {
        *self.broadcast.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        *self.worktrees.lock().unwrap_or_else(|e| e.into_inner()) = Some(worktrees);
    }

    fn sender(&self) -> Option<broadcast::Sender<BroadcastMsg>> {
        self.broadcast
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether any event subscriber opted into card doorbells.
    fn subscribed(&self) -> bool {
        self.sender().is_some_and(|tx| tx.receiver_count() > 0)
            && crate::daemon_protocol::agent_card_subscribers() > 0
    }

    /// The resolved catalog, re-read from the user config when the file
    /// changed since the last read.
    pub fn catalog(&self) -> Arc<ResolvedCatalog> {
        let path = crate::config::user_config_path();
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (path.clone(), m.modified().ok(), m.len()));
        let stamp = stamp.or(Some((path, None, u64::MAX)));
        {
            let inner = self.lock();
            if let Some(cached) = &inner.catalog
                && cached.stamp == stamp
            {
                return Arc::clone(&cached.catalog);
            }
        }
        let catalog = Arc::new(card_catalog::resolve_from_user_config());
        self.lock().catalog = Some(CachedCatalog {
            stamp,
            catalog: Arc::clone(&catalog),
        });
        catalog
    }

    /// Store `report` for the generation `agent_id` in `pane_id`, replacing
    /// whatever it published before, and clearing answered questions. The
    /// deliveries stay: what the user sent outlives the report that offered
    /// it. The report must already be validated.
    pub fn publish(&self, pane_id: &str, agent_id: &str, report: AgentReport) {
        let mut inner = self.lock();
        inner.epoch += 1;
        let epoch = inner.epoch;
        let slot = inner.slots.entry(entry_key(pane_id, agent_id)).or_default();
        let publishes = slot.report.as_ref().map_or(0, |r| r.publishes) + 1;
        slot.report = Some(StoredReport {
            report,
            published_at: Utc::now(),
            publishes,
        });
        slot.answered.clear();
        slot.epoch = epoch;
    }

    fn record_delivery(&self, key: &str, record: CardDeliveryRecord) {
        let mut inner = self.lock();
        inner.epoch += 1;
        let epoch = inner.epoch;
        let slot = inner.slots.entry(key.to_string()).or_default();
        if let Some((question_id, option_id)) = record
            .action_id
            .strip_prefix("question/")
            .and_then(|rest| rest.split_once('/'))
        {
            slot.answered.insert(
                question_id.to_string(),
                CardAnswered {
                    option_id: option_id.to_string(),
                    action_id: record.action_id.clone(),
                    at: record.at,
                },
            );
        }
        slot.push_delivery(record);
        slot.epoch = epoch;
    }

    /// Claim `request_id` for one invocation, atomically: the first claim
    /// runs it, a repeat of the same request waits for or reuses its answer,
    /// and a repeat naming different arguments is refused.
    fn claim(&self, request_id: &str, fingerprint: [u8; 32]) -> Claim {
        let mut inner = self.lock();
        let now = Instant::now();
        inner.forget_expired(now);
        match inner.requests.get(request_id) {
            Some(Remembered::InFlight {
                fingerprint: f,
                answer,
                ..
            }) if *f == fingerprint => return Claim::Wait(answer.clone()),
            Some(Remembered::Done {
                fingerprint: f,
                result,
                ..
            }) if *f == fingerprint => return Claim::Answered(result.clone()),
            Some(_) => {
                return Claim::Answered(CardActionResult::refused(
                    request_id,
                    CODE_REQUEST_REUSED,
                    "this request id was already used for a different request; nothing was done",
                ));
            }
            None => {}
        }
        inner.make_room(request_id.len());
        let (tx, rx) = tokio::sync::watch::channel(None);
        inner.requests.insert(
            request_id.to_string(),
            Remembered::InFlight {
                fingerprint,
                at: now,
                answer: rx,
            },
        );
        Claim::Run(tx)
    }

    /// Remember the answer of the invocation that claimed `request_id`.
    fn finish(&self, request_id: &str, fingerprint: [u8; 32], result: &CardActionResult) {
        let mut inner = self.lock();
        let done = Remembered::Done {
            fingerprint,
            at: Instant::now(),
            result: result.clone(),
        };
        let incoming = request_id.len() + done.bytes();
        inner.requests.remove(request_id);
        inner.make_room(incoming);
        inner.requests.insert(request_id.to_string(), done);
    }
}

/// Map an inbox message state to a card delivery state.
fn delivery_state(summary: &crate::inbox::MessageSummary) -> (String, Option<String>) {
    use crate::inbox::StateName;
    match summary.state {
        StateName::Queued | StateName::Fetched | StateName::Claimed => ("queued".into(), None),
        StateName::Acknowledged => ("delivered".into(), None),
        StateName::Undeliverable => (
            "not_delivered".into(),
            summary
                .undeliverable_reason
                .and_then(|r| serde_json::to_value(r).ok())
                .and_then(|v| v.as_str().map(str::to_string)),
        ),
    }
}

/// Gather the facts about every agent generation the deck still holds — live,
/// or exited on its own and not yet closed.
async fn gather(
    registry: &Arc<AgentPtyRegistry>,
    state: &crate::state::SharedState,
) -> (Vec<AgentFacts>, Vec<UnitFacts>) {
    let mut records: Vec<(AgentRecord, bool)> = registry
        .agent_records_including_exited()
        .into_iter()
        .filter(|(record, _, unconfirmed)| !unconfirmed && record.pane_id_env.is_some())
        .map(|(record, exited, _)| (record, exited))
        .collect();
    {
        let guard = state.read().await;
        for (record, _) in &mut records {
            record.live = guard.live_session_for(&record.id, record.pane_id_env.as_deref());
        }
    }
    let worktrees = registry
        .agent_cards()
        .worktrees
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let inbox = registry.inbox();
    let agents = records
        .into_iter()
        .map(|(record, exited)| {
            let pane_id = record.pane_id_env.clone().unwrap_or_default();
            let summary = inbox.summary(&pane_id, &record.id);
            let has_worktree = worktrees.as_ref().is_some_and(|w| {
                crate::issue_dispatch_run::worktree_of_record(&record).is_some_and(|path| {
                    w.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains_key(&path)
                })
            });
            AgentFacts {
                stopped: exited || record.crashed == Some(true),
                undelivered_messages: summary.queued + summary.fetched,
                has_worktree,
                pane_id,
                record,
            }
        })
        .collect();
    let units = registry
        .dispatched_units()
        .live()
        .map(|u| UnitFacts {
            dispatcher_pane: u.dispatcher.pane_id.clone(),
            dispatcher_agent: u.dispatcher.agent_id.clone(),
            single: match &u.kind {
                crate::dispatched_units::UnitKind::Single { pane_id, agent_id } => {
                    Some((pane_id.clone(), agent_id.clone()))
                }
                crate::dispatched_units::UnitKind::Orchestration { .. } => None,
            },
            orchestration_id: u.kind.orchestration_id().map(str::to_string),
        })
        .collect();
    (agents, units)
}

/// The inbox state of every message a card recorded as delivered, by
/// message id.
fn delivery_states(registry: &Arc<AgentPtyRegistry>) -> HashMap<String, (String, Option<String>)> {
    let wanted: HashSet<String> = {
        let inner = registry.agent_cards().lock();
        inner
            .slots
            .iter()
            .filter(|(_, s)| !s.deliveries.is_empty())
            .filter_map(|(k, _)| split_key(k).map(|(p, _)| p.to_string()))
            .collect()
    };
    let mut states = HashMap::new();
    for pane in wanted {
        for summary in registry.inbox().list(&pane) {
            states.insert(summary.id.to_string(), delivery_state(&summary));
        }
    }
    states
}

/// A doorbell to ring once the store's lock is released.
struct Doorbell {
    key: String,
    revision: u64,
    removed: bool,
}

fn own_instance_id() -> String {
    crate::daemon_protocol::try_daemon_instance_id()
        .unwrap_or_default()
        .to_string()
}

/// Recompute every entry, assign revisions to the ones that changed, ring
/// their doorbells (at most four per entry per second), and answer the list.
///
/// Called by `list-agent-cards`, after a publish and after an action, and by
/// the daemon's watcher while a subscriber opted into card doorbells, so a
/// change the deck notices on its own (a status, an exit, a close) is
/// announced within a second.
pub async fn refresh(
    registry: &Arc<AgentPtyRegistry>,
    state: &crate::state::SharedState,
) -> AgentCardList {
    let _turn = registry.agent_cards().serial.lock().await;
    refresh_in_turn(registry, state).await
}

/// [`refresh`] by a caller that already holds the store's turn.
async fn refresh_in_turn(
    registry: &Arc<AgentPtyRegistry>,
    state: &crate::state::SharedState,
) -> AgentCardList {
    let store = registry.agent_cards();
    let catalog = store.catalog();
    let instance_id = own_instance_id();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let read_at = store.lock().epoch;
        let (agents, units) = gather(registry, state).await;
        let states = delivery_states(registry);
        let committed = store.lock().commit(
            &agents,
            &units,
            &states,
            &catalog,
            &instance_id,
            read_at,
            attempt < REFRESH_ATTEMPTS,
            Utc::now(),
            Instant::now(),
        );
        let Some((list, doorbells)) = committed else {
            continue;
        };
        if let Some(tx) = store.sender() {
            for bell in doorbells {
                // Errs only when nobody is subscribed, which is not a failure.
                let _ = tx.send(BroadcastMsg::Event(doorbell_event(&bell)));
            }
        }
        return list;
    }
}

fn doorbell_event(bell: &Doorbell) -> crate::event::AgentEvent {
    let (pane, agent) = split_key(&bell.key).unwrap_or((bell.key.as_str(), ""));
    let mut metadata = HashMap::new();
    metadata.insert(AGENT_CARD_KEY_METADATA_KEY.to_string(), bell.key.clone());
    metadata.insert(
        AGENT_CARD_REVISION_METADATA_KEY.to_string(),
        bell.revision.to_string(),
    );
    if bell.removed {
        metadata.insert(AGENT_CARD_REMOVED_METADATA_KEY.to_string(), "1".to_string());
    }
    crate::event::AgentEvent {
        session_id: format!("agent-card:{}", bell.key),
        // The daemon speaking, not the agent.
        agent_type: crate::event::AgentType::None,
        event_type: crate::event::EventType::AgentCard,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: Utc::now(),
        user_prompt: None,
        metadata,
        pane_id: Some(pane.to_string()),
        agent_id: (!agent.is_empty()).then(|| agent.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    }
}

/// Whether `msg` is a card doorbell: forwarded only to a subscriber that
/// opted into them (review B1).
pub fn is_doorbell(msg: &BroadcastMsg) -> bool {
    matches!(msg, BroadcastMsg::Event(e) if e.event_type == crate::event::EventType::AgentCard)
}

/// The daemon's watcher: recompute the entries every [`WATCH_INTERVAL`] while
/// a subscriber opted into card doorbells, so every change to an entry rings
/// its doorbell within a second.
pub fn spawn_watcher(
    registry: Arc<AgentPtyRegistry>,
    state: crate::state::SharedState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(WATCH_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if registry.agent_cards().subscribed() {
                refresh(&registry, &state).await;
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

/// Validate and store a report the hook socket admitted for `agent_id` in
/// `pane_id`, and answer the reply.
pub async fn publish(
    registry: &Arc<AgentPtyRegistry>,
    state: &crate::state::SharedState,
    pane_id: &str,
    agent_id: &str,
    report: AgentReport,
) -> crate::event::CardPublishReply {
    let store = registry.agent_cards();
    let catalog = store.catalog();
    if let Err(error) = report.validate(&catalog) {
        return crate::event::CardPublishReply::refused(error.code, &error.message);
    }
    let key = entry_key(pane_id, agent_id);
    let list = {
        let _turn = store.serial.lock().await;
        store.publish(pane_id, agent_id, report);
        refresh_in_turn(registry, state).await
    };
    let revision = list
        .entries
        .iter()
        .find(|e| e.key == key)
        .map(|e| e.revision);
    crate::event::CardPublishReply {
        accepted: true,
        key: Some(key),
        revision,
        code: None,
        message: None,
    }
}

// ---------------------------------------------------------------------------
// Acting
// ---------------------------------------------------------------------------

/// `invoke-card-action` as the attach socket receives it.
#[derive(Debug, Clone)]
pub struct CardActionRequest {
    pub pane_id: String,
    pub agent_id: String,
    pub action_id: String,
    /// The `token` of the action definition the user was shown.
    pub action_token: String,
    /// The `instance_id` of the daemon process that listed the card.
    pub instance_id: String,
    pub request_id: String,
    pub confirm: bool,
    pub text: Option<String>,
}

/// What a close action reports: the worktree kept, if one was.
pub type CloseOutcome = Result<Option<String>, String>;

fn is_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// Refuse a request whose identifiers are malformed or oversized before
/// anything about it is remembered (audit F4).
fn check_request(request: &CardActionRequest) -> Result<(), Box<CardActionResult>> {
    if !is_request_id(&request.request_id) {
        return Err(Box::new(CardActionResult::refused(
            "",
            CODE_REQUEST_INVALID,
            format!(
                "the request id must be 1 to {MAX_REQUEST_ID_BYTES} letters, digits, '.', '_', ':' or '-'"
            ),
        )));
    }
    for (what, value) in [
        ("pane id", &request.pane_id),
        ("agent id", &request.agent_id),
        ("action id", &request.action_id),
        ("action token", &request.action_token),
        ("instance id", &request.instance_id),
    ] {
        if value.len() > MAX_REQUEST_FIELD_BYTES {
            return Err(Box::new(CardActionResult::refused(
                &request.request_id,
                CODE_REQUEST_INVALID,
                format!("the {what} is longer than {MAX_REQUEST_FIELD_BYTES} bytes"),
            )));
        }
    }
    if request
        .text
        .as_ref()
        .is_some_and(|t| t.len() > MAX_REPLY_TEXT_BYTES)
    {
        return Err(Box::new(CardActionResult::refused(
            &request.request_id,
            CODE_TEXT_REQUIRED,
            format!(
                "reply text must be at most {MAX_REPLY_TEXT_BYTES} bytes without control characters"
            ),
        )));
    }
    Ok(())
}

/// A digest of everything a request asks for, so a repeat of its id can be
/// told apart from a different request reusing it.
fn request_fingerprint(request: &CardActionRequest) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let bytes = serde_json::to_vec(&(
        &request.pane_id,
        &request.agent_id,
        &request.action_id,
        &request.action_token,
        &request.instance_id,
        request.confirm,
        &request.text,
    ))
    .unwrap_or_default();
    Sha256::digest(&bytes).into()
}

/// Invoke one action of one entry. `close` performs the deck's stop of an
/// agent id (`keep_worktree` for `close-keep-worktree`); the attach handler
/// passes its own stop so a card close is the same close the desktop row's
/// Close sends.
///
/// The `request_id` is claimed atomically before anything runs: a repeat of
/// the same request while the first is still running waits for its answer,
/// one after it gets that answer for [`REQUEST_ID_MEMORY`], and neither does
/// anything again (review B5, audit F6). The invocation runs on its own task,
/// so a client that hangs up neither cancels an effect already under way nor
/// leaves the id claimed with no answer.
pub async fn invoke<F, Fut>(
    registry: &Arc<AgentPtyRegistry>,
    state: &crate::state::SharedState,
    request: CardActionRequest,
    close: F,
) -> CardActionResult
where
    F: FnOnce(String, bool) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = CloseOutcome> + Send + 'static,
{
    if let Err(refusal) = check_request(&request) {
        return *refusal;
    }
    let fingerprint = request_fingerprint(&request);
    let rid = request.request_id.clone();
    let registry = Arc::clone(registry);
    let state = state.clone();
    run_once(registry.agent_cards(), &rid, fingerprint, {
        let registry = Arc::clone(&registry);
        async move { invoke_once(&registry, &state, &request, close).await }
    })
    .await
}

/// Run `work` for `request_id` unless a request with that id ran or is
/// running: the claim, the detached run and the answer every repeat gets.
async fn run_once<W>(
    store: &CardStore,
    request_id: &str,
    fingerprint: [u8; 32],
    work: W,
) -> CardActionResult
where
    W: std::future::Future<Output = CardActionResult> + Send + 'static,
{
    match store.claim(request_id, fingerprint) {
        Claim::Answered(result) => result,
        Claim::Wait(mut answer) => match answer.wait_for(Option::is_some).await {
            Ok(result) => result.clone().unwrap_or_else(|| interrupted(request_id)),
            Err(_) => interrupted(request_id),
        },
        Claim::Run(tx) => {
            let task = tokio::spawn(work);
            let result = task.await.unwrap_or_else(|_| interrupted(request_id));
            store.finish(request_id, fingerprint, &result);
            let _ = tx.send(Some(result.clone()));
            result
        }
    }
}

fn interrupted(request_id: &str) -> CardActionResult {
    CardActionResult::refused(
        request_id,
        CODE_DELIVERY_REFUSED,
        "the deck could not finish this action; list the cards to see whether it took effect",
    )
}

async fn invoke_once<F, Fut>(
    registry: &Arc<AgentPtyRegistry>,
    state: &crate::state::SharedState,
    request: &CardActionRequest,
    close: F,
) -> CardActionResult
where
    F: FnOnce(String, bool) -> Fut,
    Fut: std::future::Future<Output = CloseOutcome>,
{
    let rid = request.request_id.as_str();
    // The ids a card names are this daemon's: another process may number an
    // unrelated agent the same (review B3).
    if request.instance_id != own_instance_id() {
        return CardActionResult::refused(
            rid,
            CODE_DAEMON_REPLACED,
            "the deck's daemon was restarted after this card was read; list the cards again",
        );
    }
    let key = entry_key(&request.pane_id, &request.agent_id);
    let store = registry.agent_cards();
    let turn = store.serial.lock().await;
    let list = refresh_in_turn(registry, state).await;
    let Some(entry) = list.entries.iter().find(|e| e.key == key) else {
        let pane_held = list.entries.iter().any(|e| e.pane_id == request.pane_id);
        return if pane_held {
            CardActionResult::refused(
                rid,
                CODE_STALE_ENTRY,
                format!(
                    "the agent in pane {} was replaced after this card was read; list the cards again",
                    request.pane_id
                ),
            )
        } else {
            CardActionResult::refused(rid, CODE_UNKNOWN_ENTRY, format!("there is no card {key}"))
        };
    };
    let Some(action) = entry.action(&request.action_id) else {
        return CardActionResult::refused(
            rid,
            CODE_UNKNOWN_ACTION,
            format!("card {key} has no action {}", request.action_id),
        );
    };
    // The definition the user was shown, and nothing else, is what a press
    // sends (review B2, audit F1).
    if request.action_token != action.token {
        let sentence = if request.action_token.is_empty() {
            format!(
                "the request names no version of action {}; list the cards and pass its token",
                request.action_id
            )
        } else {
            format!(
                "action {} changed after this card was read, so nothing was sent; list the cards again and review it",
                request.action_id
            )
        };
        return CardActionResult::refused(rid, CODE_STALE_ACTION, sentence);
    }
    if action.kind == "open" {
        return CardActionResult::refused(
            rid,
            CODE_CLIENT_SIDE_ACTION,
            "open is done by the app, not the daemon",
        );
    }
    if !action.enabled {
        let reason = match action.disabled_reason.as_deref() {
            Some(REASON_AGENT_STOPPED) => "the agent is not running",
            _ => "it is not available now",
        };
        return CardActionResult::refused(
            rid,
            CODE_ACTION_DISABLED,
            format!("{} is disabled: {reason}", action.label),
        );
    }
    if action.confirm && !request.confirm {
        return CardActionResult::refused(
            rid,
            CODE_CONFIRM_REQUIRED,
            format!(
                "{} asks for confirmation; repeat with --confirm",
                action.label
            ),
        );
    }
    let entry_name = entry.name.clone();
    match action.kind.as_str() {
        "close" | "close_keep_worktree" => {
            let keep = action.kind == "close_keep_worktree";
            // The stop waits for the process; the store's turn is not held
            // across it, so the cards stay readable meanwhile.
            drop(turn);
            match close(request.agent_id.clone(), keep).await {
                Ok(kept) => {
                    let list = refresh(registry, state).await;
                    CardActionResult {
                        request_id: rid.to_string(),
                        outcome: "closed".into(),
                        message_id: None,
                        kept_worktree: kept,
                        code: None,
                        message: Some(entry_name),
                        entry_revision: Some(list.revision),
                    }
                }
                Err(message) => CardActionResult::refused(rid, CODE_CLOSE_FAILED, message),
            }
        }
        _ => {
            let body = if action.text_input {
                match request.text.as_deref() {
                    Some(text) if !text.trim().is_empty() => {
                        if text.len() > MAX_REPLY_TEXT_BYTES || has_disallowed_control(text, true) {
                            return CardActionResult::refused(
                                rid,
                                CODE_TEXT_REQUIRED,
                                format!(
                                    "reply text must be at most {MAX_REPLY_TEXT_BYTES} bytes without control characters"
                                ),
                            );
                        }
                        text.to_string()
                    }
                    _ => {
                        return CardActionResult::refused(
                            rid,
                            CODE_TEXT_REQUIRED,
                            "reply needs --text",
                        );
                    }
                }
            } else {
                match action.prompt.clone() {
                    Some(prompt) if !prompt.trim().is_empty() => prompt,
                    _ => {
                        return CardActionResult::refused(
                            rid,
                            CODE_ACTION_DISABLED,
                            format!("{} is disabled: it has no prompt to send", action.label),
                        );
                    }
                }
            };
            let subject = format!("card action: {}", action.label);
            match post_card_message(
                registry,
                &request.pane_id,
                &request.agent_id,
                subject,
                body.clone(),
            ) {
                Ok(message_id) => {
                    store.record_delivery(
                        &key,
                        CardDeliveryRecord {
                            action_id: action.id.clone(),
                            label: action.label.clone(),
                            text: body,
                            token: action.token.clone(),
                            outcome: "sent".into(),
                            message_id: message_id.clone(),
                            state: "queued".into(),
                            detail: None,
                            at: Utc::now(),
                        },
                    );
                    let list = refresh_in_turn(registry, state).await;
                    CardActionResult {
                        request_id: rid.to_string(),
                        outcome: "sent".into(),
                        message_id: Some(message_id),
                        kept_worktree: None,
                        code: None,
                        message: Some(entry_name),
                        entry_revision: list
                            .entries
                            .iter()
                            .find(|e| e.key == key)
                            .map(|e| e.revision),
                    }
                }
                Err(message) => CardActionResult::refused(rid, CODE_DELIVERY_REFUSED, message),
            }
        }
    }
}

/// Post a card action's text to the inbox of the agent generation in
/// `pane_id`, bound to `agent_id`, through the same steps the attach
/// `post-message` verb takes: refused unless that generation still holds the
/// pane, and checked again under the inbox's lock right before it goes in.
fn post_card_message(
    registry: &Arc<AgentPtyRegistry>,
    pane_id: &str,
    agent_id: &str,
    subject: String,
    body: String,
) -> Result<String, String> {
    let Some(live) = registry
        .pane_current_agent_id(pane_id)
        .filter(|live| live == agent_id)
    else {
        return Err(format!(
            "the agent in pane {pane_id} is no longer running; nothing was sent"
        ));
    };
    let mut content = crate::inbox::Content {
        from: crate::inbox::Sender::Deck {
            reason: CARD_ACTION_POST_REASON.to_string(),
        },
        subject,
        body,
        body_trust: crate::inbox::BodyTrust::Trusted,
        spill_path: None,
        id: None,
        preface: None,
        provenance: None,
    };
    let to = crate::inbox::Recipient::bound(pane_id, live.clone());
    registry
        .inbox()
        .admit_direct(&to, &content)
        .map_err(|e| format!("the deck's inbox did not take the message: {e}"))?;
    let dirs = crate::inbox_hub::spill_dirs(registry, pane_id, Some(live.as_str()));
    let deck = registry.inbox().deck();
    let spill = crate::inbox_hub::off_the_executor(|| {
        crate::inbox_hub::retain_full_body_minted(&deck, pane_id, &mut content, &dirs)
    })
    .map_err(|e| format!("the deck's inbox did not take the message: {e}"))?;
    let still_live = || registry.pane_current_agent_id(pane_id).as_deref() == Some(live.as_str());
    let id = registry
        .inbox()
        .post_direct_where(to, content, &still_live)
        .map_err(|e| format!("the deck's inbox did not take the message: {e}"))?;
    registry.inbox().own_spill(spill, &id);
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card_catalog::resolve;

    fn report(json: &str) -> AgentReport {
        AgentReport::parse_json(json).unwrap()
    }

    #[test]
    fn a_full_report_parses_and_validates() {
        let r = report(
            r##"{
              "status": "waiting",
              "summary": "one line",
              "fields": [ { "name": "pull-request", "value": "#123" },
                          { "name": "build-time", "value": 42, "type": "number", "label": "Build time" } ],
              "questions": [ { "id": "base", "text": "Which base branch?",
                               "options": [ { "id": "main", "label": "main" },
                                            { "id": "rel", "label": "release", "prompt": "Use the release branch." } ] } ],
              "actions": [ { "name": "merge" },
                           { "name": "rerun", "label": "Re-run tests", "prompt": "Re-run the failing tests." } ]
            }"##,
        );
        assert_eq!(r.validate(&resolve(None)), Ok(()));
        assert_eq!(r.status, Some(ReportStatus::Waiting));
        let toml = AgentReport::parse_toml(
            "status = \"done\"\nsummary = \"s\"\n[[fields]]\nname = \"progress\"\nvalue = \"x\"\n",
        )
        .unwrap();
        assert_eq!(toml.validate(&resolve(None)), Ok(()));
        assert_eq!(
            AgentReport::parse_json("{}").unwrap(),
            AgentReport::default()
        );
    }

    #[test]
    fn unknown_keys_and_bad_status_are_refused() {
        let e = AgentReport::parse_json(r#"{"stauts": "done"}"#).unwrap_err();
        assert_eq!(e.code, CODE_REPORT_INVALID);
        let e = AgentReport::parse_json(r#"{"status": "sleeping"}"#).unwrap_err();
        assert_eq!(e.code, CODE_REPORT_INVALID);
        let e = AgentReport::parse_json(&" ".repeat(MAX_REPORT_BYTES + 1)).unwrap_err();
        assert_eq!(e.code, CODE_REPORT_TOO_LARGE);
    }

    fn invalid(json: &str) -> String {
        let r = report(json);
        let e = r.validate(&resolve(None)).unwrap_err();
        assert_eq!(e.code, CODE_REPORT_INVALID, "{}", e.message);
        e.message
    }

    #[test]
    fn validation_names_what_is_wrong() {
        assert!(invalid(&format!(r#"{{"summary": "{}"}}"#, "x".repeat(301))).contains("summary"));
        assert!(invalid(r#"{"summary": "a\nb"}"#).contains("line break"));
        let many: Vec<String> = (0..33)
            .map(|i| format!(r#"{{"name": "f{i}", "value": "v"}}"#))
            .collect();
        assert!(invalid(&format!(r#"{{"fields": [{}]}}"#, many.join(","))).contains("33 fields"));
        assert!(invalid(r#"{"fields": [{"name": "status", "value": "x"}]}"#).contains("reserved"));
        assert!(
            invalid(r#"{"fields": [{"name": "pull-request", "value": 12}]}"#)
                .contains("field \"pull-request\" must be a string")
        );
        assert!(
            invalid(r#"{"fields": [{"name": "x", "value": "y", "type": "colour"}]}"#)
                .contains("unknown type")
        );
        assert!(
            invalid(r#"{"fields": [{"name": "u", "value": "ftp://x", "type": "url"}]}"#)
                .contains("http")
        );
        let options: Vec<String> = (0..9)
            .map(|i| format!(r#"{{"id": "o{i}", "label": "l"}}"#))
            .collect();
        assert!(
            invalid(&format!(
                r#"{{"questions": [{{"id": "q", "text": "t", "options": [{}]}}]}}"#,
                options.join(",")
            ))
            .contains("1 to 8 options")
        );
        assert!(invalid(r#"{"actions": [{"name": "close"}]}"#).contains("reserved"));
        assert!(invalid(r#"{"actions": [{"name": "questionable"}]}"#).contains("reserved"));
        assert!(
            invalid(r#"{"actions": [{"name": "merge", "prompt": "x"}]}"#)
                .contains("catalog action")
        );
        assert!(invalid(r#"{"actions": [{"name": "zap", "prompt": "x"}]}"#).contains("label"));
        assert!(
            invalid(
                r#"{"actions": [{"name": "zap", "label": "Zap", "prompt": "x", "confirm": true}]}"#
            )
            .contains("confirmation")
        );
    }

    #[test]
    fn a_custom_field_defaults_to_text() {
        let r = report(r#"{"fields": [{"name": "risk", "value": "low"}]}"#);
        assert_eq!(r.validate(&resolve(None)), Ok(()));
        let r = report(r#"{"fields": [{"name": "risk", "value": 3}]}"#);
        assert!(r.validate(&resolve(None)).is_err());
    }

    #[test]
    fn references() {
        for ok in ["#1", "owner/repo#123", "https://github.com/a/b/pull/1"] {
            assert!(is_reference(ok), "{ok}");
        }
        for bad in ["123", "#", "#x", "a#1", "a/b/c#1", "http://x"] {
            assert!(!is_reference(bad), "{bad}");
        }
    }

    fn record(id: &str, pane: &str) -> AgentRecord {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "pane_id_env": pane,
        }))
        .unwrap()
    }

    fn facts(id: &str, pane: &str) -> AgentFacts {
        AgentFacts {
            record: record(id, pane),
            pane_id: pane.to_string(),
            stopped: false,
            undelivered_messages: 0,
            has_worktree: false,
        }
    }

    fn slot_with(json: &str) -> Slot {
        Slot {
            report: Some(StoredReport {
                report: report(json),
                published_at: Utc::now(),
                publishes: 1,
            }),
            ..Slot::default()
        }
    }

    #[test]
    fn an_entry_resolves_actions_needs_you_and_primary() {
        let catalog = resolve(None);
        let mut slots = HashMap::new();
        slots.insert(
            "p1/1".to_string(),
            slot_with(
                r##"{"status": "waiting", "summary": "s",
                     "fields": [{"name": "risk", "value": "low"}, {"name": "progress", "value": "x"}],
                     "questions": [{"id": "base", "text": "Which?", "options": [{"id": "main", "label": "main"}]}],
                     "actions": [{"name": "zap", "label": "Zap", "prompt": "Do zap."}, {"name": "merge"}]}"##,
            ),
        );
        let entries = project_all(
            &[facts("1", "p1"), facts("2", "p2")],
            &slots,
            &catalog,
            &[],
            "i",
        );
        let e = &entries[0];
        assert_eq!(e.key, "p1/1");
        assert_eq!(e.needs_you_reasons, ["question", "waiting"]);
        assert_eq!(e.fields[0].name, "progress");
        assert!(!e.fields[0].custom);
        assert_eq!(e.fields[1].name, "risk");
        assert!(e.fields[1].custom);
        assert_eq!(e.fields[1].placement, "detail");
        let ids: Vec<&str> = e.actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "question/base/main",
                "merge",
                "explain",
                "zap",
                "reply",
                "open",
                "close"
            ]
        );
        assert_eq!(
            e.action("question/base/main").unwrap().prompt.as_deref(),
            Some("Answer to your question \"Which?\": main")
        );
        assert_eq!(e.primary_action_id, "question/base/main");
        assert!(e.action("zap").unwrap().custom);
        assert_eq!(
            e.action("close").unwrap().confirm_detail.as_deref(),
            Some("Still pending: an unanswered question.")
        );
        let other = &entries[1];
        assert!(other.report.is_none());
        assert!(!other.needs_you);
        assert_eq!(other.primary_action_id, "open");
        assert_eq!(other.group.kind, "standalone");
    }

    #[test]
    fn a_stopped_agent_disables_prompt_actions() {
        let catalog = resolve(None);
        let mut f = facts("1", "p1");
        f.stopped = true;
        f.has_worktree = true;
        let entries = project_all(&[f], &HashMap::new(), &catalog, &[], "i");
        let e = &entries[0];
        assert_eq!(e.status, "stopped");
        assert_eq!(e.needs_you_reasons, ["stopped"]);
        for a in &e.actions {
            if a.kind == "prompt" {
                assert!(!a.enabled);
                assert_eq!(a.disabled_reason.as_deref(), Some(REASON_AGENT_STOPPED));
            } else {
                assert!(a.enabled);
            }
        }
        assert!(e.action("close-keep-worktree").is_some());
    }

    #[test]
    fn dispatcher_groups_order_needs_you_first() {
        let catalog = resolve(None);
        let units = vec![UnitFacts {
            dispatcher_pane: "p1".into(),
            dispatcher_agent: "1".into(),
            single: Some(("p3".into(), "3".into())),
            orchestration_id: None,
        }];
        let mut slots = HashMap::new();
        slots.insert("p3/3".to_string(), slot_with(r#"{"status": "done"}"#));
        let entries = project_all(
            &[facts("1", "p1"), facts("2", "p2"), facts("3", "p3")],
            &slots,
            &catalog,
            &units,
            "i",
        );
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["p3/3", "p1/1", "p2/2"]);
        assert_eq!(entries[0].group.id, "dispatcher:p1/1");
        assert_eq!(entries[1].group.id, "dispatcher:p1/1");
        assert_eq!(
            entries[0].relations.dispatcher.as_ref().unwrap().key,
            "p1/1"
        );
        assert_eq!(entries[2].group.id, "standalone");
    }

    #[test]
    fn the_wire_additions_are_optional_and_omitted_when_empty() {
        use crate::daemon_protocol::{AttachRequest, AttachResponse};
        let request = AttachRequest::InvokeCardAction {
            pane_id: "p".into(),
            agent_id: "1".into(),
            action_id: "merge".into(),
            action_token: "act-1".into(),
            instance_id: "i".into(),
            request_id: "r".into(),
            confirm: false,
            text: None,
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["op"], "invoke-card-action");
        assert!(value.get("confirm").is_none());
        assert!(value.get("text").is_none());
        assert_eq!(
            serde_json::to_value(AttachRequest::ListAgentCards).unwrap()["op"],
            "list-agent-cards"
        );
        assert_eq!(
            serde_json::to_value(AttachRequest::AgentCardCatalog).unwrap()["op"],
            "agent-card-catalog"
        );
        // An answer without the new keys decodes, and a plain answer carries none.
        let plain: AttachResponse = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(plain.agent_cards.is_none() && plain.card_catalog.is_none());
        let json = serde_json::to_value(AttachResponse::ok()).unwrap();
        assert!(json.get("agent_cards").is_none() && json.get("card_action").is_none());
        let reply: crate::event::HookCapabilitiesReply = serde_json::from_str("{}").unwrap();
        assert_eq!(reply.agent_card, None);
        assert!(crate::hook::confirms_agent_card("{}").is_err());
        assert!(crate::hook::confirms_agent_card(r#"{"agent_card":1}"#).is_ok());
        let ty: crate::event::EventType = serde_json::from_str("\"agent_card\"").unwrap();
        assert_eq!(ty, crate::event::EventType::AgentCard);
        let ty: crate::event::EventType = serde_json::from_str("\"agent_cardz\"").unwrap();
        assert_eq!(ty, crate::event::EventType::Unknown);
        assert!(crate::daemon_protocol::DAEMON_CAPABILITIES.contains(&"agent-cards"));
    }

    fn one(entries: &[AgentCardEntry], key: &str) -> AgentCardEntry {
        entries.iter().find(|e| e.key == key).cloned().unwrap()
    }

    /// Review B2 / audit F1: an action's token names its definition — the
    /// text it sends, its label, its confirmation and the question an option
    /// answers — and nothing else, so an unrelated change to the card keeps
    /// it and a changed definition never does.
    #[test]
    fn an_action_token_changes_with_its_definition_and_nothing_else() {
        let catalog = resolve(None);
        let base = r#"{"questions": [{"id": "q", "text": "Which?", "options": [{"id": "a", "label": "A"}]}],
                       "actions": [{"name": "go", "label": "Go", "prompt": "Run the tests."}]}"#;
        let token = |json: &str, stopped: bool, instance: &str| {
            let mut slots = HashMap::new();
            slots.insert("p1/1".to_string(), slot_with(json));
            let mut f = facts("1", "p1");
            f.stopped = stopped;
            let e = one(&project_all(&[f], &slots, &catalog, &[], instance), "p1/1");
            (
                e.action("go").map(|a| a.token.clone()),
                e.action("question/q/a").unwrap().token.clone(),
                e.action("close").unwrap().token.clone(),
            )
        };
        let (go, option, close) = token(base, false, "i");
        assert!(go.as_deref().is_some_and(|t| t.starts_with("act-")));
        assert_eq!(
            token(base, false, "i"),
            (go.clone(), option.clone(), close.clone())
        );
        // A status the deck noticed changes no definition.
        let (stopped_go, _, stopped_close) = token(base, true, "i");
        assert_eq!(stopped_go, go);
        assert_eq!(stopped_close, close);
        // Another prompt or label under the same id is another definition.
        assert_ne!(
            token(
                &base.replace("Run the tests.", "Delete the branch."),
                false,
                "i"
            )
            .0,
            go
        );
        assert_ne!(token(&base.replace("\"Go\"", "\"Go!\""), false, "i").0, go);
        // So is the same option under another question.
        assert_ne!(
            token(&base.replace("Which?", "Which one?"), false, "i").1,
            option
        );
        // And the same card listed by another daemon process.
        assert_ne!(token(base, false, "j").2, close);
    }

    /// Review B7: removing a catalog action a report suggested by name takes
    /// it off the card (it has no text of its own); a field whose value no
    /// longer fits the type the catalog now gives it is not shown; a field the
    /// catalog dropped is shown as the agent's own, with the type it gave.
    #[test]
    fn a_changed_catalog_is_applied_to_reports_already_published() {
        let published = r#"{"fields": [{"name": "progress", "value": "half"}, {"name": "risk", "value": 3, "type": "number"}],
                            "actions": [{"name": "merge"}, {"name": "zap", "label": "Zap", "prompt": "Do zap."}]}"#;
        let mut slots = HashMap::new();
        slots.insert("p1/1".to_string(), slot_with(published));
        let defaults = resolve(None);
        // `risk` was published as a catalog field by a user catalog naming it text.
        let with_risk = resolve(Some(
            "[[catalog.fields]]\nname = \"risk\"\ndescription = \"d\"\ntype = \"text\"\n",
        ));
        let e = one(
            &project_all(&[facts("1", "p1")], &slots, &defaults, &[], "i"),
            "p1/1",
        );
        assert!(e.action("merge").is_some());
        assert!(
            e.fields
                .iter()
                .any(|f| f.name == "risk" && f.custom && f.field_type == "number")
        );

        let removed = resolve(Some("[catalog]\nremove = [\"merge\"]\n"));
        let e = one(
            &project_all(&[facts("1", "p1")], &slots, &removed, &[], "i"),
            "p1/1",
        );
        assert!(
            e.action("merge").is_none(),
            "no empty custom action: {:?}",
            e.actions
        );
        assert!(
            e.action("zap")
                .is_some_and(|a| a.prompt.as_deref() == Some("Do zap."))
        );
        assert_ne!(e.primary_action_id, "merge");
        for a in &e.actions {
            if a.kind == "prompt" && !a.text_input {
                assert!(a.prompt.as_deref().is_some_and(|p| !p.is_empty()), "{a:?}");
            }
        }

        let e = one(
            &project_all(&[facts("1", "p1")], &slots, &with_risk, &[], "i"),
            "p1/1",
        );
        assert!(
            !e.fields.iter().any(|f| f.name == "risk"),
            "a number is not a text value: {:?}",
            e.fields
        );
        assert!(e.fields.iter().any(|f| f.name == "progress"));
    }

    /// Product decision (a) and review S1: what was sent stays on the card
    /// after a new report, newest first; an action shows as its last delivery
    /// only a delivery of its current definition.
    #[test]
    fn deliveries_outlive_the_report_and_belong_to_one_definition() {
        let catalog = resolve(None);
        let store = CardStore::default();
        store.publish(
            "p1",
            "1",
            report(r#"{"actions": [{"name": "go", "label": "Go", "prompt": "One."}]}"#),
        );
        let project = |store: &CardStore| {
            let inner = store.lock();
            one(
                &project_all(&[facts("1", "p1")], &inner.slots, &catalog, &[], "i"),
                "p1/1",
            )
        };
        let first = project(&store).action("go").unwrap().clone();
        store.record_delivery(
            "p1/1",
            CardDeliveryRecord {
                action_id: "go".into(),
                label: "Go".into(),
                text: "One.".into(),
                token: first.token.clone(),
                outcome: "sent".into(),
                message_id: "m-1".into(),
                state: "queued".into(),
                detail: None,
                at: Utc::now(),
            },
        );
        assert_eq!(
            project(&store)
                .action("go")
                .unwrap()
                .last_delivery
                .as_ref()
                .map(|d| d.message_id.as_str()),
            Some("m-1")
        );
        // The same id, another text: not this definition's delivery.
        store.publish(
            "p1",
            "1",
            report(r#"{"actions": [{"name": "go", "label": "Go", "prompt": "Two."}]}"#),
        );
        let e = project(&store);
        assert!(e.action("go").unwrap().last_delivery.is_none());
        assert_eq!(e.recent_deliveries.len(), 1);
        assert_eq!(e.recent_deliveries[0].text, "One.");
        // A report that no longer offers it keeps the record too.
        store.publish("p1", "1", AgentReport::default());
        let e = project(&store);
        assert!(e.action("go").is_none());
        assert_eq!(e.recent_deliveries[0].message_id, "m-1");
    }

    /// Audit F5: an entry keeps a bounded number and size of deliveries,
    /// however many actions are invoked on it.
    #[test]
    fn recent_deliveries_are_bounded_by_count_and_bytes() {
        let mut slot = Slot::default();
        let record = |n: usize, text: String| CardDeliveryRecord {
            action_id: format!("a{n}"),
            label: "L".into(),
            text,
            token: format!("t{n}"),
            outcome: "sent".into(),
            message_id: format!("m-{n}"),
            state: "queued".into(),
            detail: None,
            at: Utc::now(),
        };
        for n in 0..100 {
            slot.push_delivery(record(n, "short".into()));
        }
        assert_eq!(slot.deliveries.len(), MAX_RECENT_DELIVERIES);
        assert_eq!(slot.deliveries[0].message_id, "m-99", "newest first");
        for n in 100..110 {
            slot.push_delivery(record(n, "x".repeat(MAX_REPLY_TEXT_BYTES)));
        }
        let bytes: usize = slot
            .deliveries
            .iter()
            .map(|d| d.text.len() + d.label.len() + d.action_id.len())
            .sum();
        assert!(bytes <= MAX_RECENT_DELIVERY_BYTES, "{bytes}");
        assert_eq!(slot.deliveries[0].message_id, "m-109");
    }

    /// Review B4: a refresh that read the registry before a new generation
    /// published (its snapshot has no such agent) neither prunes that
    /// generation's report nor commits: it asks to read again. Without a
    /// retry left it still keeps the report.
    #[test]
    fn a_stale_reading_never_prunes_a_report_published_after_it() {
        let catalog = resolve(None);
        let store = CardStore::default();
        // A refresh reads the deck: only p1/1 is live.
        let read_at = store.lock().epoch;
        let stale = [facts("1", "p1")];
        // Meanwhile p2/2 spawns and publishes.
        store.publish("p2", "2", report(r#"{"summary": "fresh"}"#));
        let now = Utc::now();
        let instant = Instant::now();
        let mut inner = store.lock();
        let retried = inner.commit(
            &stale,
            &[],
            &HashMap::new(),
            &catalog,
            "i",
            read_at,
            true,
            now,
            instant,
        );
        assert!(
            retried.is_none(),
            "a reading older than a publish is read again"
        );
        assert!(inner.slots.contains_key("p2/2"));
        let (list, _) = inner
            .commit(
                &stale,
                &[],
                &HashMap::new(),
                &catalog,
                "i",
                read_at,
                false,
                now,
                instant,
            )
            .unwrap();
        assert!(
            inner.slots.contains_key("p2/2"),
            "kept by the last attempt too"
        );
        assert_eq!(list.entries.len(), 1);
        // A reading taken after the publish that still lacks p2/2 (it was
        // closed) prunes it.
        let read_at = inner.epoch;
        inner
            .commit(
                &stale,
                &[],
                &HashMap::new(),
                &catalog,
                "i",
                read_at,
                true,
                now,
                instant,
            )
            .unwrap();
        assert!(!inner.slots.contains_key("p2/2"));
    }

    /// Review B5 / audit F6: two overlapping invocations with one request id
    /// run the work once and get the same answer; a later repeat gets it from
    /// memory; the id reused for a different request is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_request_id_runs_once_however_its_repeats_overlap() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let store = Arc::new(CardStore::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let work = {
            let runs = Arc::clone(&runs);
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                let _ = released.await;
                CardActionResult {
                    request_id: "card-1".into(),
                    outcome: "sent".into(),
                    message_id: Some("m-only".into()),
                    kept_worktree: None,
                    code: None,
                    message: None,
                    entry_revision: Some(1),
                }
            }
        };
        let first = tokio::spawn({
            let store = Arc::clone(&store);
            async move { run_once(&store, "card-1", [1; 32], work).await }
        });
        while runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // The repeat arrives while the first is still running.
        let second = tokio::spawn({
            let store = Arc::clone(&store);
            let runs = Arc::clone(&runs);
            async move {
                run_once(&store, "card-1", [1; 32], async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    CardActionResult::refused("card-1", "x", "a second run")
                })
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).unwrap();
        let (a, b) = (first.await.unwrap(), second.await.unwrap());
        assert_eq!(a, b);
        assert_eq!(a.message_id.as_deref(), Some("m-only"));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the work ran once");
        let third = run_once(&store, "card-1", [1; 32], async {
            CardActionResult::refused("card-1", "x", "a third run")
        })
        .await;
        assert_eq!(third, a);
        let reused = run_once(&store, "card-1", [2; 32], async {
            CardActionResult::refused("card-1", "x", "never")
        })
        .await;
        assert_eq!(reused.code.as_deref(), Some(CODE_REQUEST_REUSED));
    }

    /// Audit F4: a request with an oversized or odd request id, or an
    /// oversized identifier, is refused before anything about it is kept;
    /// and what is kept stays within its byte budget.
    #[tokio::test]
    async fn malformed_request_ids_are_refused_and_never_remembered() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: crate::state::SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let request = |request_id: String, pane_id: String| CardActionRequest {
            pane_id,
            agent_id: "1".into(),
            action_id: "merge".into(),
            action_token: "t".into(),
            instance_id: own_instance_id(),
            request_id,
            confirm: false,
            text: None,
        };
        let close = |_: String, _: bool| async { Ok(None) };
        for (rid, pane) in [
            ("x".repeat(8 * 1024 * 1024), "p".to_string()),
            (String::new(), "p".to_string()),
            ("card 1".to_string(), "p".to_string()),
            (
                "card-ok".to_string(),
                "p".repeat(MAX_REQUEST_FIELD_BYTES + 1),
            ),
        ] {
            let result = invoke(&registry, &state, request(rid, pane), close).await;
            assert_eq!(
                result.code.as_deref(),
                Some(CODE_REQUEST_INVALID),
                "{result:?}"
            );
            assert!(result.request_id.len() <= MAX_REQUEST_ID_BYTES);
        }
        assert!(registry.agent_cards().lock().requests.is_empty());
        // Well-formed ids are remembered within the budget, however many.
        for n in 0..(MAX_REMEMBERED_REQUESTS + 10) {
            let result = invoke(
                &registry,
                &state,
                request(format!("card-{n}"), "p".into()),
                close,
            )
            .await;
            assert_eq!(result.code.as_deref(), Some(CODE_UNKNOWN_ENTRY));
        }
        let inner = registry.agent_cards().lock();
        assert!(inner.requests.len() <= MAX_REMEMBERED_REQUESTS);
        let bytes: usize = inner
            .requests
            .iter()
            .map(|(k, r)| k.len() + r.bytes())
            .sum();
        assert!(bytes <= MAX_REMEMBERED_BYTES);
    }

    /// Review B3: an invocation naming another daemon process is refused
    /// before anything is resolved.
    #[tokio::test]
    async fn an_invocation_from_another_daemon_process_is_refused() {
        let registry = Arc::new(AgentPtyRegistry::new());
        let state: crate::state::SharedState =
            Arc::new(tokio::sync::RwLock::new(crate::state::AppState::default()));
        let request = CardActionRequest {
            pane_id: "p".into(),
            agent_id: "1".into(),
            action_id: "merge".into(),
            action_token: "t".into(),
            instance_id: "a-daemon-that-was-replaced".into(),
            request_id: "card-b3".into(),
            confirm: false,
            text: None,
        };
        let result = invoke(&registry, &state, request, |_: String, _: bool| async {
            Ok(None)
        })
        .await;
        assert_eq!(result.code.as_deref(), Some(CODE_DAEMON_REPLACED));
    }

    /// Review B1: a subscription that did not opt in — every subscriber
    /// older than cards, and the TUI — is never sent a card doorbell, which
    /// an older reader would apply to a session as activity; one that opted
    /// in is. The request a client sent before the flag existed still decodes
    /// and means "no doorbells", and the flag is omitted when off.
    #[test]
    fn card_doorbells_reach_only_subscriptions_that_opted_in() {
        use crate::daemon_protocol::{AttachRequest, event_frame};
        let bell = BroadcastMsg::Event(doorbell_event(&Doorbell {
            key: "p1/1".into(),
            revision: 3,
            removed: false,
        }));
        assert!(is_doorbell(&bell));
        assert!(
            event_frame(&bell, false).is_none(),
            "an un-opted subscriber gets none"
        );
        let sent = event_frame(&bell, true).unwrap().unwrap();
        assert!(String::from_utf8(sent).unwrap().contains("agent_card"));
        let other: crate::event::AgentEvent = serde_json::from_value(serde_json::json!({
            "session_id": "s", "agent_type": "claude_code", "event_type": "tool_start",
            "timestamp": "2026-11-17T12:00:00Z", "metadata": {},
        }))
        .unwrap();
        assert!(event_frame(&BroadcastMsg::Event(other), false).is_some());

        let old: AttachRequest = serde_json::from_str(r#"{"op":"subscribe-events"}"#).unwrap();
        assert!(matches!(
            old,
            AttachRequest::SubscribeEvents { agent_cards: false }
        ));
        assert_eq!(
            serde_json::to_string(&AttachRequest::SubscribeEvents { agent_cards: false }).unwrap(),
            r#"{"op":"subscribe-events"}"#
        );
        let opted: AttachRequest =
            serde_json::from_str(r#"{"op":"subscribe-events","agent_cards":true}"#).unwrap();
        assert!(matches!(
            opted,
            AttachRequest::SubscribeEvents { agent_cards: true }
        ));
        // A request from before the token and instance existed decodes, and
        // is refused by the daemon rather than failing to parse.
        let old: AttachRequest = serde_json::from_str(
            r#"{"op":"invoke-card-action","pane_id":"p","agent_id":"1","action_id":"merge","request_id":"r"}"#,
        )
        .unwrap();
        assert!(matches!(
            old,
            AttachRequest::InvokeCardAction { ref action_token, ref instance_id, .. }
                if action_token.is_empty() && instance_id.is_empty()
        ));
    }

    #[test]
    fn keys_split_at_the_last_slash() {
        assert_eq!(split_key("p3/a7"), Some(("p3", "a7")));
        assert_eq!(split_key("a/b/c"), Some(("a/b", "c")));
        assert_eq!(split_key("p3"), None);
        assert_eq!(split_key("/a"), None);
    }
}
