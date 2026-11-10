//! PRD #1885: the desktop half of the trust prompt, `Review project commands`.
//!
//! Before the app starts an orchestration from a project's config — the New
//! agent dialog's orchestration launch and the Runs screen's Activate — the
//! webview asks the deck that launch goes to whether that orchestration's
//! commands and prompts are approved as they now read
//! ([`desktop_project_trust_state`]). An approved, unchanged one starts as
//! before. Otherwise the webview shows the review dialog, and the user's
//! **Approve and start** approves exactly the text shown
//! ([`desktop_approve_project_trust`], carrying the digest of that text) before
//! anything starts. The daemon owns the decision and the store; nothing here
//! reads a project's config or the approval store itself (that would be a
//! client-side project read, which `xtask/linkage-check`'s desktop boundary
//! refuses, and against a remote deck it would read the wrong machine).
//!
//! # Every piece of config text is escaped HERE
//!
//! A project's commands, descriptions and prompts are untrusted text. Each is
//! passed through `untrusted_text::escape_control_and_bidi` — the function the
//! TUI's prompt draws through — so a control character or a bidi override is
//! shown as its escape (`\u{1b}`, `\u{202e}`) in both clients, never obeyed. The
//! multi-line fields arrive as one escaped string per line, split the way the
//! TUI splits them. The webview renders all of it as plain text.
//!
//! # An older deck
//!
//! A deck that does not advertise the trust requests is older than this app and
//! gates nothing itself. This app cannot check that deck's approvals — they
//! live on the deck's host, and reading them here is the client-side read the
//! boundary forbids — so it never starts an orchestration there silently: it
//! answers [`DesktopProjectTrust::Unsupported`] with a refusal naming the
//! approving command, and the webview starts nothing.

use dot_agent_deck::agent_pty::is_valid_display_name;
use dot_agent_deck::daemon_client::GatedQuery;
use dot_agent_deck::untrusted_text::escape_control_and_bidi;
use serde::Serialize;
use tauri::{State, Webview};

use crate::dto::{safe_message, validate_pasted_project_path};
use crate::terminal::DesktopState;

/// The longest digest accepted from the webview. The daemon's is a SHA-256 in
/// lowercase hex, 64 characters; anything longer is not one.
const DIGEST_MAX_CHARS: usize = 128;

/// The deck's answer about one orchestration's commands and prompts, or why it
/// cannot give one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopProjectTrust {
    /// The deck's view: approved, or what the dialog shows.
    Review(DesktopProjectTrustView),
    /// The deck predates the trust requests. `reason` is the refusal the
    /// webview shows instead of starting anything.
    Unsupported { reason: String },
}

/// What the review dialog shows. Every string except `state`, `digest` and a
/// role's `change` is config or path text, already escaped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopProjectTrustView {
    /// The project the approval is recorded under (a git repository's main
    /// working tree).
    pub project: String,
    /// The directory the config was read from, when it is not `project` (a
    /// linked worktree).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
    pub orchestration: String,
    /// `approved`, `never_approved` or `changed`.
    pub state: String,
    pub roles: Vec<DesktopTrustRole>,
    /// Roles the approval had that the orchestration no longer has.
    pub removed_roles: Vec<String>,
    /// The digest of exactly this text, which the approval carries back.
    pub digest: String,
}

/// One role as the dialog shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopTrustRole {
    pub name: String,
    /// `changed` or `new` relative to the stored approval; absent when the
    /// role is unchanged or nothing was ever approved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// One escaped string per line; absent when the role has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<Vec<String>>,
    /// One escaped string per line; absent when the role has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Vec<String>>,
}

/// A multi-line config field, one escaped string per line — the TUI's split
/// (`str::lines`), so a `\r\n` file reads the same in both clients.
fn escaped_lines(text: &str) -> Vec<String> {
    let lines: Vec<String> = text.lines().map(escape_control_and_bidi).collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

/// The deck's view, as the dialog shows it. Takes the fields by name rather
/// than the crate type, whose module is outside what the desktop may import.
macro_rules! review_from_view {
    ($view:expr) => {{
        let view = $view;
        let config_dir =
            (view.config_dir != view.project).then(|| escape_control_and_bidi(&view.config_dir));
        DesktopProjectTrustView {
            project: escape_control_and_bidi(&view.project),
            config_dir,
            orchestration: escape_control_and_bidi(&view.orchestration),
            state: view.state.clone(),
            roles: view
                .roles
                .iter()
                .map(|role| DesktopTrustRole {
                    name: escape_control_and_bidi(&role.text.name),
                    change: role.change.map(|change| change.word().to_string()),
                    command: escape_control_and_bidi(&role.text.command),
                    agent: role.text.agent.as_deref().map(escape_control_and_bidi),
                    description: role.text.description.as_deref().map(escaped_lines),
                    prompt: role.text.prompt_template.as_deref().map(escaped_lines),
                })
                .collect(),
            removed_roles: view
                .removed_roles
                .iter()
                .map(|name| escape_control_and_bidi(name))
                .collect(),
            digest: view.digest.clone(),
        }
    }};
}

/// The refusal for a deck too old to answer trust requests. Names the command
/// that approves the orchestration on the deck's host, as the daemon's own
/// refusals do.
pub(crate) fn older_deck_refusal(path: &str, orchestration: &str) -> String {
    safe_message(format!(
        "This deck's daemon is older than this app and cannot show the review of the \
         orchestration's commands and prompts, so nothing was started. Upgrade that deck's \
         daemon, or on its host review and approve them with `dot-agent-deck trust approve \
         {} --orchestration {}` and start the orchestration from the TUI there.",
        escape_control_and_bidi(path),
        escape_control_and_bidi(orchestration),
    ))
}

fn validate_target(path: &str, orchestration: &str) -> Result<(), String> {
    validate_pasted_project_path(path)?;
    if !is_valid_display_name(orchestration) {
        return Err(
            "orchestration name is invalid, oversized, empty, or contains control characters"
                .into(),
        );
    }
    Ok(())
}

/// The deck a review is for: the one `deck_id` names (the New agent dialog's),
/// or, without one, the selected deck as the Runs screen's launch takes it —
/// refused under All Decks, exactly as that launch is.
async fn review_daemon(
    state: &DesktopState,
    deck_id: Option<&str>,
) -> Result<std::sync::Arc<crate::daemon_bridge::TrustedDaemon>, String> {
    let daemon = match deck_id {
        Some(deck_id) => {
            let scope = crate::dto::DeckScope::resolve(Some(deck_id))?;
            state.daemon.trusted(scope.endpoint()).await?
        }
        None => crate::daemon_bridge::trusted_daemon(&state.daemon).await?,
    };
    daemon.require_compatible()?;
    Ok(daemon)
}

/// [`desktop_project_trust_state`] minus the webview.
pub(crate) async fn project_trust_state_on(
    state: &DesktopState,
    deck_id: Option<&str>,
    path: &str,
    orchestration: &str,
) -> Result<DesktopProjectTrust, String> {
    validate_target(path, orchestration)?;
    let daemon = review_daemon(state, deck_id).await?;
    let answer = crate::daemon_bridge::bounded_reply(
        "ProjectTrustState",
        daemon.client.project_trust_state(path, orchestration),
    )
    .await?;
    Ok(match answer {
        GatedQuery::Answered(view) => DesktopProjectTrust::Review(review_from_view!(&view)),
        GatedQuery::Unsupported => DesktopProjectTrust::Unsupported {
            reason: older_deck_refusal(path, orchestration),
        },
    })
}

/// [`desktop_approve_project_trust`] minus the webview.
pub(crate) async fn approve_project_trust_on(
    state: &DesktopState,
    deck_id: Option<&str>,
    path: &str,
    orchestration: &str,
    digest: &str,
) -> Result<DesktopProjectTrust, String> {
    validate_target(path, orchestration)?;
    if digest.is_empty()
        || digest.chars().count() > DIGEST_MAX_CHARS
        || !digest.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err("the approval names no digest of the text that was shown".into());
    }
    let daemon = review_daemon(state, deck_id).await?;
    let answer = crate::daemon_bridge::bounded_reply(
        "ApproveProjectTrust",
        daemon
            .client
            .approve_project_trust(path, orchestration, digest),
    )
    .await?;
    Ok(match answer {
        GatedQuery::Answered(view) => DesktopProjectTrust::Review(review_from_view!(&view)),
        GatedQuery::Unsupported => DesktopProjectTrust::Unsupported {
            reason: older_deck_refusal(path, orchestration),
        },
    })
}

/// PRD #1885: whether `orchestration` of the project in `path` is approved on
/// the deck `deck_id` names (the selected deck when absent, as the Runs
/// screen's launch takes it), and the text the review dialog shows when it is
/// not. Read-only.
#[tauri::command]
pub(crate) async fn desktop_project_trust_state(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: Option<String>,
    path: String,
    orchestration: String,
) -> Result<DesktopProjectTrust, String> {
    crate::ensure_main_webview(&webview)?;
    project_trust_state_on(&state, deck_id.as_deref(), &path, &orchestration).await
}

/// PRD #1885: approve `orchestration` of the project in `path` on the user's
/// **Approve and start**, carrying the `digest` of the text the dialog showed.
/// The deck records exactly that text, or refuses with `trust-text-changed`
/// when the config changed since, and the dialog shows the new text.
#[tauri::command]
pub(crate) async fn desktop_approve_project_trust(
    webview: Webview,
    state: State<'_, DesktopState>,
    deck_id: Option<String>,
    path: String,
    orchestration: String,
    digest: String,
) -> Result<DesktopProjectTrust, String> {
    crate::ensure_main_webview(&webview)?;
    approve_project_trust_on(&state, deck_id.as_deref(), &path, &orchestration, &digest).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words the webview's dialog shows must be the TUI's (CLAUDE.md rule
    /// 22). The TUI's live in `dot_agent_deck::trust_prompt`; the webview's
    /// copy is `desktop/src/lib/projectTrust.ts`, which this reads, so a change
    /// to either that the other does not follow fails here.
    #[test]
    fn the_review_dialog_uses_the_tui_prompts_words() {
        let words = include_str!("../../src/lib/projectTrust.ts");
        for (what, word) in [
            ("title", dot_agent_deck::trust_prompt::TITLE),
            (
                "never approved",
                dot_agent_deck::trust_prompt::NOT_APPROVED_BEFORE,
            ),
            (
                "changed",
                dot_agent_deck::trust_prompt::CHANGED_SINCE_APPROVAL,
            ),
        ] {
            assert!(
                words.contains(&format!("\"{word}\"")),
                "projectTrust.ts must carry the TUI's {what} word {word:?}"
            );
        }
    }

    /// Escaped one line at a time, as the TUI draws them: an ESC and a bidi
    /// override are shown, a line break splits.
    #[test]
    fn multi_line_fields_arrive_as_escaped_lines() {
        assert_eq!(
            escaped_lines("first \u{1b}[2J\r\nsecond \u{202e}x"),
            vec![
                "first \\u{1b}[2J".to_string(),
                "second \\u{202e}x".to_string()
            ]
        );
        assert_eq!(escaped_lines(""), vec![String::new()]);
    }

    /// The older-deck refusal names the approving command, and never lets a
    /// hostile path paint the message.
    #[test]
    fn the_older_deck_refusal_names_the_approving_command() {
        let reason = older_deck_refusal("/srv/demo\u{1b}]0;x\u{7}", "review");
        assert!(reason.contains("dot-agent-deck trust approve"), "{reason}");
        assert!(reason.contains("--orchestration review"), "{reason}");
        assert!(reason.contains("nothing was started"), "{reason}");
        assert!(!reason.contains('\u{1b}'), "{reason}");
    }

    /// A digest that is not one is refused before any deck is contacted.
    #[tokio::test]
    async fn an_approval_without_a_real_digest_is_refused_before_any_deck() {
        let state = DesktopState::default();
        for digest in ["", "not-hex!", &"a".repeat(DIGEST_MAX_CHARS + 1)] {
            let refused = approve_project_trust_on(
                &state,
                Some("deck-nowhere"),
                "/srv/demo",
                "review",
                digest,
            )
            .await;
            assert_eq!(
                refused,
                Err("the approval names no digest of the text that was shown".to_string()),
                "{digest:?}"
            );
        }
    }
}
