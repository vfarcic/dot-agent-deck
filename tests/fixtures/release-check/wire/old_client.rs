//! Touched reader shapes frozen from 7c788760; see the adjacent README.
#![allow(dead_code)]

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HoldCause {
    Filesystem,
    WritableDirectory,
    ForeignOwner,
    Acl,
    ExportFile,
    Io,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct HoldDetail {
    pub cause: HoldCause,
    #[serde(default, deserialize_with = "bounded_detail")]
    pub path_component: Option<String>,
    #[serde(default, deserialize_with = "bounded_detail")]
    pub filesystem: Option<String>,
}

fn bounded_detail<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let text: Option<String> = Deserialize::deserialize(deserializer)?;
    Ok(text.map(|text| text.chars().take(256).collect()))
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WakeHold {
    PossibleDraft,
    AwaitingPrompt,
    PasteOpen,
    CannotType,
    NotTaken,
    UncertainInput,
    DialogDuringRing,
    NeedsFetch,
    CannotCheck,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct InboxSummary {
    pub queued: usize,
    pub fetched: usize,
    #[serde(default)]
    pub claimed: usize,
    pub undeliverable: usize,
    #[serde(default)]
    pub last_acked_at: Option<Value>,
    pub wake: Value,
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub wake_stalled: bool,
    #[serde(default)]
    pub wake_hold: Option<WakeHold>,
    #[serde(default)]
    pub hold_detail: Option<HoldDetail>,
    #[serde(flatten, default)]
    pub notices: InboxCardNotices,
}

#[derive(Debug, Default, Deserialize)]
pub struct InboxCardNotices {
    #[serde(default)]
    pub attention: Vec<Value>,
    #[serde(default)]
    pub integration: Option<Value>,
    #[serde(default)]
    pub integration_command: Option<String>,
    #[serde(default)]
    pub receipts_degraded: Option<String>,
    #[serde(default)]
    pub evidence_paused: Option<usize>,
    #[serde(default)]
    pub admission_refused: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AttachResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub inbox_messages: Option<Vec<Value>>,
}

impl InboxSummary {
    /// Frozen projection of CardInbox::hold_text for CannotType. The old
    /// hold_words consults a detail only for CannotCheck, so even a known
    /// detail is ignored alongside CannotType.
    pub fn cannot_type_notice(&self) -> Option<String> {
        if self.queued + self.fetched > 0 && self.wake_hold == Some(WakeHold::CannotType) {
            Some(format!(
                "✉{} waiting for the agent to fetch it. Send it a prompt.",
                self.queued + self.fetched
            ))
        } else {
            None
        }
    }
}
