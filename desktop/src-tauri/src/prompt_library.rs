//! Issue #824: the prompt library, read from and written to the deck that
//! keeps it.
//!
//! The library used to live in the webview's `localStorage`, on the client
//! side of PRD #803's ownership boundary. It is the deck's now
//! ([`dot_agent_deck::prompt_library`]): each command here names the deck it is
//! for and asks that deck, never whichever deck happens to be selected — the
//! [`crate::dto::DeckScope::resolve`] rule every per-deck verb follows.
//!
//! A deck that predates the library answers
//! [`DesktopPromptLibrary::Unsupported`] rather than an error, and is sent
//! nothing: the webview then keeps its own copy for that deck, as it did
//! before.

use dot_agent_deck::daemon_client::GatedQuery;
use dot_agent_deck::prompt_library::Prompt;
use serde::{Deserialize, Serialize};
use tauri::{State, Webview};

use crate::terminal::DesktopState;

/// One saved prompt as the webview holds it. The text is the user's own,
/// passed through verbatim in both directions: the webview renders it only
/// into text nodes and form fields, never as markup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopPrompt {
    pub id: String,
    pub name: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl From<Prompt> for DesktopPrompt {
    fn from(prompt: Prompt) -> Self {
        Self {
            id: prompt.id,
            name: prompt.name,
            body: prompt.body,
            note: prompt.note,
        }
    }
}

impl From<DesktopPrompt> for Prompt {
    fn from(prompt: DesktopPrompt) -> Self {
        Self {
            id: prompt.id,
            name: prompt.name,
            body: prompt.body,
            note: prompt.note,
        }
    }
}

/// A deck's prompt library, or the deck's answer that it keeps none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum DesktopPromptLibrary {
    /// The deck keeps the library, and this is it as it stands, in order.
    Deck { prompts: Vec<DesktopPrompt> },
    /// The deck predates the library (it does not advertise
    /// `prompt-library`) and was sent nothing.
    Unsupported,
}

impl From<GatedQuery<Vec<Prompt>>> for DesktopPromptLibrary {
    fn from(answer: GatedQuery<Vec<Prompt>>) -> Self {
        match answer {
            GatedQuery::Answered(prompts) => Self::Deck {
                prompts: prompts.into_iter().map(DesktopPrompt::from).collect(),
            },
            GatedQuery::Unsupported => Self::Unsupported,
        }
    }
}

/// The four verbs, as one value, so [`prompt_library_on`] is one code path.
pub(crate) enum PromptLibraryVerb {
    List,
    Save(DesktopPrompt),
    Delete(String),
    Seed(Vec<DesktopPrompt>),
}

/// [`desktop_prompt_library`] minus the webview, so a test can drive it
/// against real daemons. The deck comes from the request, through
/// [`crate::dto::DeckScope::resolve`], so a deck that left the fleet is
/// refused rather than another one asked.
pub(crate) async fn prompt_library_on(
    state: &DesktopState,
    deck_id: &str,
    verb: PromptLibraryVerb,
) -> Result<DesktopPromptLibrary, String> {
    let scope = crate::dto::DeckScope::resolve(Some(deck_id))?;
    let daemon = state.daemon.trusted(scope.endpoint()).await?;
    daemon.require_compatible()?;
    let client = &daemon.client;
    let answer = match verb {
        PromptLibraryVerb::List => {
            crate::daemon_bridge::bounded_reply("ListPrompts", client.list_prompts()).await?
        }
        PromptLibraryVerb::Save(prompt) => {
            crate::daemon_bridge::bounded_reply("SavePrompt", client.save_prompt(prompt.into()))
                .await?
        }
        PromptLibraryVerb::Delete(id) => {
            crate::daemon_bridge::bounded_reply("DeletePrompt", client.delete_prompt(&id)).await?
        }
        PromptLibraryVerb::Seed(prompts) => {
            crate::daemon_bridge::bounded_reply(
                "SeedPrompts",
                client.seed_prompts(prompts.into_iter().map(Prompt::from).collect()),
            )
            .await?
        }
    };
    Ok(answer.into())
}

/// Issue #824: the prompt library of the deck `deck_id` names.
#[tauri::command]
pub(crate) async fn desktop_list_prompts(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: String,
) -> Result<DesktopPromptLibrary, String> {
    crate::ensure_main_webview(&webview)?;
    prompt_library_on(&state, &deck_id, PromptLibraryVerb::List).await
}

/// Issue #824: add `prompt` to that deck's library, or replace the prompt with
/// its id, answering the library afterwards. A prompt over the deck's limits
/// is refused in the deck's own sentence, which names the limit.
#[tauri::command]
pub(crate) async fn desktop_save_prompt(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: String,
    prompt: DesktopPrompt,
) -> Result<DesktopPromptLibrary, String> {
    crate::ensure_main_webview(&webview)?;
    prompt_library_on(&state, &deck_id, PromptLibraryVerb::Save(prompt)).await
}

/// Issue #824: remove the prompt with `id` from that deck's library.
#[tauri::command]
pub(crate) async fn desktop_delete_prompt(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: String,
    id: String,
) -> Result<DesktopPromptLibrary, String> {
    crate::ensure_main_webview(&webview)?;
    prompt_library_on(&state, &deck_id, PromptLibraryVerb::Delete(id)).await
}

/// Issue #824: hand the library this app kept before decks owned one to the
/// deck `deck_id` names. The deck adds the prompts it does not hold and
/// replaces none, all or nothing; the webview removes its own copy only once
/// this answers `Deck`.
#[tauri::command]
pub(crate) async fn desktop_seed_prompts(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: String,
    prompts: Vec<DesktopPrompt>,
) -> Result<DesktopPromptLibrary, String> {
    crate::ensure_main_webview(&webview)?;
    prompt_library_on(&state, &deck_id, PromptLibraryVerb::Seed(prompts)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape the webview's `normalizePromptLibrary` reads: a `kind`
    /// tag, camelCase fields, and an absent note omitted.
    #[test]
    fn the_library_serialises_in_the_shape_the_webview_reads() {
        let deck = DesktopPromptLibrary::Deck {
            prompts: vec![DesktopPrompt {
                id: "p".into(),
                name: "n".into(),
                body: "b".into(),
                note: None,
            }],
        };
        assert_eq!(
            serde_json::to_value(&deck).unwrap(),
            serde_json::json!({ "kind": "deck", "prompts": [{ "id": "p", "name": "n", "body": "b" }] })
        );
        assert_eq!(
            serde_json::to_value(DesktopPromptLibrary::Unsupported).unwrap(),
            serde_json::json!({ "kind": "unsupported" })
        );
    }
}
