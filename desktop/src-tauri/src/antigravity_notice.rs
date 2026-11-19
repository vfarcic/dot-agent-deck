//! PRD #211: the desktop app's Antigravity experimental notice, the TUI's
//! notice in the desktop's own dialog.
//!
//! **One state, shared with the TUI.** Both clients read and write the same
//! per-user file, `config::AntigravityNoticeState` at
//! `config::antigravity_notice_path` (`antigravity-notice-state.json` in the
//! user's config directory, or the file `DOT_AGENT_DECK_ANTIGRAVITY_NOTICE`
//! names). An Antigravity start from either client's New agent flow counts
//! towards the next showing, and Don't show again in either stops both. Like
//! the star reminder (`star_prompt.rs`), it is the user's own file on this
//! machine and moves nothing on any daemon.
//!
//! **What counts.** The webview asks [`desktop_antigravity_notice_start`]
//! after the New agent dialog's start succeeded, with the command it started.
//! The agent is told apart by `AgentType::from_command`, the inference the
//! TUI's New Agent submit and this crate's `start_agent_action` both make, so
//! the two clients count the same starts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use dot_agent_deck::antigravity_notice::{new_issue_url, open_issues_message};
use dot_agent_deck::config::{AntigravityNoticeState, antigravity_notice_path};
use dot_agent_deck::event::AgentType;
use dot_agent_deck::system_browser::{LAUNCH_GRACE, open_with_platform_opener};
use serde::Deserialize;
use tauri::Webview;

/// The dialog's three choices, as the webview names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeChoice {
    /// Open issues (`o`): open the repository's new-issue page.
    Open,
    /// Later (`l`, `Esc`): show again at the next interval.
    Later,
    /// Don't show again (`d`): never show it again.
    Dismiss,
}

/// Whether this process has said why the state file could not be read or
/// saved: once is enough, as the star reminder says it once at startup.
static PROBLEM_REPORTED: AtomicBool = AtomicBool::new(false);

/// Whether `command` starts Antigravity, as the TUI decides it.
pub fn starts_antigravity(command: Option<&str>) -> bool {
    AgentType::from_command(command) == Some(AgentType::Antigravity)
}

/// Count a New agent start of `command` in the file at `path`, and answer
/// whether to show the notice for it. A start of any other agent counts
/// nothing and shows nothing. Why the file could not be read or saved goes to
/// stderr, the first time only.
pub fn record_start(path: &Path, command: Option<&str>) -> bool {
    if !starts_antigravity(command) {
        return false;
    }
    let start = AntigravityNoticeState::record_start(path);
    if let Some(problem) = start.problem
        && !PROBLEM_REPORTED.swap(true, Ordering::SeqCst)
    {
        eprintln!("{problem}");
    }
    start.show
}

/// Record `choice` in the file at `path`. Only Don't show again saves
/// anything: the showing was recorded with the start, so Later and Open
/// issues leave the next showing where the interval puts it. The error says
/// why it could not be saved.
pub fn record_choice(path: &Path, choice: NoticeChoice) -> Result<(), String> {
    match choice {
        NoticeChoice::Dismiss => AntigravityNoticeState::dismiss_permanently(path),
        NoticeChoice::Open | NoticeChoice::Later => Ok(()),
    }
}

/// A New agent start of `command` succeeded: whether to show the notice.
#[tauri::command]
pub async fn desktop_antigravity_notice_start(
    webview: Webview,
    command: Option<String>,
) -> Result<bool, String> {
    crate::ensure_main_webview(&webview)?;
    let path: PathBuf = antigravity_notice_path();
    tokio::task::spawn_blocking(move || record_start(&path, command.as_deref()))
        .await
        .map_err(|error| format!("Could not read the Antigravity notice's state: {error}"))
}

/// The user's choice in the notice. Answers the sentence to show for Open
/// issues — thanks, or where to go when the browser did not open — and
/// nothing for the other two, as the TUI reports them. A Don't show again
/// that could not be saved is an error, so the webview can show the notice
/// again.
#[tauri::command]
pub async fn desktop_antigravity_notice_choose(
    webview: Webview,
    choice: NoticeChoice,
) -> Result<Option<String>, String> {
    crate::ensure_main_webview(&webview)?;
    let path = antigravity_notice_path();
    tokio::task::spawn_blocking(move || {
        record_choice(&path, choice)?;
        Ok((choice == NoticeChoice::Open).then(|| {
            open_issues_message(open_with_platform_opener(&new_issue_url(), LAUNCH_GRACE).is_ok())
        }))
    })
    .await
    .map_err(|error| format!("Could not save the Antigravity notice's choice: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::antigravity_notice::NOTICE_INTERVAL;

    /// The TUI's start, as `ui.rs` makes it after a successful New Agent
    /// submit whose command is Antigravity's.
    fn tui_start(path: &Path) -> bool {
        AntigravityNoticeState::record_start(path).show
    }

    fn desktop_start(path: &Path) -> bool {
        record_start(path, Some("agy"))
    }

    fn starts(path: &Path) -> u64 {
        AntigravityNoticeState::load_from(path).0.start_count
    }

    #[test]
    fn only_an_antigravity_start_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        for command in [None, Some(""), Some("claude"), Some("codex"), Some("bash")] {
            assert!(!record_start(&path, command), "{command:?} showed");
        }
        assert_eq!(starts(&path), 0, "another agent's start was counted");
        assert!(record_start(&path, Some("/usr/local/bin/agy --model x")));
        assert_eq!(starts(&path), 1);
    }

    #[test]
    fn the_first_start_and_every_hundredth_after_show_whichever_client_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        assert!(desktop_start(&path), "the first start did not show");
        for start in 2..=NOTICE_INTERVAL {
            let shown = if start % 2 == 0 {
                tui_start(&path)
            } else {
                desktop_start(&path)
            };
            assert!(!shown, "start {start} showed");
        }
        assert!(
            desktop_start(&path),
            "the hundredth start after the showing did not show"
        );
    }

    #[test]
    fn dont_show_again_in_the_desktop_holds_in_the_tui() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        assert!(desktop_start(&path));
        record_choice(&path, NoticeChoice::Dismiss).unwrap();
        for start in 2..=300 {
            assert!(!tui_start(&path), "TUI start {start} showed");
        }
    }

    #[test]
    fn dont_show_again_in_the_tui_holds_in_the_desktop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        assert!(tui_start(&path));
        AntigravityNoticeState::dismiss_permanently(&path).unwrap();
        for start in 2..=300 {
            assert!(!desktop_start(&path), "desktop start {start} showed");
        }
    }

    #[test]
    fn later_and_open_issues_wait_for_the_next_interval() {
        for choice in [NoticeChoice::Later, NoticeChoice::Open] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("notice.json");
            assert!(desktop_start(&path));
            record_choice(&path, choice).unwrap();
            for start in 2..=NOTICE_INTERVAL {
                assert!(!tui_start(&path), "{choice:?}: start {start} showed");
            }
            assert!(tui_start(&path), "{choice:?}: the notice did not come back");
        }
    }

    /// A Don't show again that cannot be written is reported, not lost
    /// silently. The state's directory is a file here, so nothing can be saved.
    #[test]
    fn a_dismissal_that_cannot_be_saved_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("notice.json");
        assert!(record_choice(&path, NoticeChoice::Dismiss).is_err());
        assert!(record_choice(&path, NoticeChoice::Later).is_ok());
    }

    #[test]
    fn the_webview_names_the_three_choices() {
        for (name, choice) in [
            ("open", NoticeChoice::Open),
            ("later", NoticeChoice::Later),
            ("dismiss", NoticeChoice::Dismiss),
        ] {
            assert_eq!(
                serde_json::from_value::<NoticeChoice>(serde_json::json!(name)).unwrap(),
                choice
            );
        }
    }

    /// CLAUDE.md rule 22: the dialog says what the TUI's notice says.
    #[test]
    fn the_dialog_uses_the_tui_notices_words() {
        use dot_agent_deck::antigravity_notice::{
            BODY, DONT_SHOW_AGAIN, LATER, OPEN_ISSUES, TITLE, issues_display,
        };
        let words = include_str!("../../src/lib/antigravityNotice.ts");
        for (what, word) in [
            ("title", TITLE.to_string()),
            ("body", BODY.to_string()),
            ("open issues", OPEN_ISSUES.to_string()),
            ("later", LATER.to_string()),
            ("don't show again", DONT_SHOW_AGAIN.to_string()),
            ("issues address", issues_display()),
            ("new-issue URL", new_issue_url()),
            ("thanks", open_issues_message(true)),
            ("where to go", open_issues_message(false)),
        ] {
            assert!(
                words.contains(&format!("\"{word}\"")),
                "antigravityNotice.ts must carry the TUI's {what} words {word:?}"
            );
        }
    }
}
