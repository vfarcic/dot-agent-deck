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

use std::sync::{LazyLock, Mutex};

use dot_agent_deck::antigravity_notice::{new_issue_url, open_issues_message};
use dot_agent_deck::config::{AntigravityNoticeSession, antigravity_notice_path};
use dot_agent_deck::event::AgentType;
use dot_agent_deck::system_browser::{LAUNCH_GRACE, open_with_platform_opener};
use serde::{Deserialize, Serialize};
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

/// What a New agent start answers the webview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoticeStart {
    /// Whether to show the notice for this start.
    pub show: bool,
    /// Why the state file could not be read or saved, the first time this
    /// process meets a problem, for the webview to say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// This process's session over the shared state file: it holds what could
/// not be saved for as long as the app runs, and hands out the first problem
/// once, as the TUI's does.
static SESSION: LazyLock<Mutex<AntigravityNoticeSession>> =
    LazyLock::new(|| Mutex::new(AntigravityNoticeSession::new(antigravity_notice_path())));

/// Whether `command` starts Antigravity, as the TUI decides it.
pub fn starts_antigravity(command: Option<&str>) -> bool {
    AgentType::from_command(command) == Some(AgentType::Antigravity)
}

/// Count a New agent start of `command` in `session`, and answer whether to
/// show the notice for it. A start of any other agent counts nothing and
/// shows nothing.
pub fn record_start(session: &mut AntigravityNoticeSession, command: Option<&str>) -> NoticeStart {
    if !starts_antigravity(command) {
        return NoticeStart {
            show: false,
            problem: None,
        };
    }
    let start = session.record_start();
    NoticeStart {
        show: start.show,
        problem: start.problem,
    }
}

/// Record `choice` in `session`. Only Don't show again saves anything: the
/// showing was recorded with the start, so Later and Open issues leave the
/// next showing where the interval puts it. A Don't show again holds for the
/// rest of the session whether or not it could be saved; the answer is why it
/// could not, when that is the first problem this process has to say.
pub fn record_choice(
    session: &mut AntigravityNoticeSession,
    choice: NoticeChoice,
) -> Option<String> {
    match choice {
        NoticeChoice::Dismiss => session.dismiss_permanently(),
        NoticeChoice::Open | NoticeChoice::Later => None,
    }
}

fn with_session<T>(use_it: impl FnOnce(&mut AntigravityNoticeSession) -> T) -> T {
    // A panic while holding it left a session that is still whole: each
    // update is a fresh read plus what was not saved.
    let mut session = SESSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    use_it(&mut session)
}

/// A New agent start of `command` succeeded: whether to show the notice, and
/// the first problem with its state file to say.
#[tauri::command]
pub async fn desktop_antigravity_notice_start(
    webview: Webview,
    command: Option<String>,
) -> Result<NoticeStart, String> {
    crate::ensure_main_webview(&webview)?;
    tokio::task::spawn_blocking(move || {
        with_session(|session| record_start(session, command.as_deref()))
    })
    .await
    .map_err(|error| format!("Could not read the Antigravity notice's state: {error}"))
}

/// The user's choice in the notice. Answers the sentence to show: for Open
/// issues, thanks or where to go when the browser did not open; for a Don't
/// show again that could not be saved, why, the first time this process has
/// a problem to say; nothing otherwise, as the TUI reports them.
#[tauri::command]
pub async fn desktop_antigravity_notice_choose(
    webview: Webview,
    choice: NoticeChoice,
) -> Result<Option<String>, String> {
    crate::ensure_main_webview(&webview)?;
    tokio::task::spawn_blocking(move || {
        let problem = with_session(|session| record_choice(session, choice));
        problem.or_else(|| {
            (choice == NoticeChoice::Open).then(|| {
                open_issues_message(
                    open_with_platform_opener(&new_issue_url(), LAUNCH_GRACE).is_ok(),
                )
            })
        })
    })
    .await
    .map_err(|error| format!("Could not save the Antigravity notice's choice: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::antigravity_notice::NOTICE_INTERVAL;

    use dot_agent_deck::config::AntigravityNoticeState;
    use std::path::Path;

    fn session(path: &Path) -> AntigravityNoticeSession {
        AntigravityNoticeSession::new(path.to_path_buf())
    }

    /// The TUI's start, as `ui.rs` makes it after a successful New Agent
    /// submit whose command is Antigravity's, from a TUI just started.
    fn tui_start(path: &Path) -> bool {
        session(path).record_start().show
    }

    fn desktop_start(path: &Path) -> bool {
        record_start(&mut session(path), Some("agy")).show
    }

    fn shows(path: &Path, command: Option<&str>) -> bool {
        record_start(&mut session(path), command).show
    }

    fn starts(path: &Path) -> u64 {
        AntigravityNoticeState::load_from(path).0.start_count
    }

    #[test]
    fn only_an_antigravity_start_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        for command in [None, Some(""), Some("claude"), Some("codex"), Some("bash")] {
            assert!(!shows(&path, command), "{command:?} showed");
        }
        assert_eq!(starts(&path), 0, "another agent's start was counted");
        assert!(shows(&path, Some("/usr/local/bin/agy --model x")));
        assert_eq!(starts(&path), 1);
    }

    /// The New agent dialog announces the command it actually started
    /// (`NewAgentDialog.tsx`'s `startCommand`), so a built-in mode's empty
    /// Command field arrives already resolved to the deck's `default_command`
    /// (`resolveAuthoringCommand`), and a plain start's empty field arrives as
    /// no command, because the deck runs its default shell for it. The crate
    /// counts the first and not the second, whatever `default_command` is.
    #[test]
    fn a_start_counts_by_the_command_it_spawned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        // A plain start with an empty field: the deck's shell.
        assert!(!shows(&path, None));
        assert_eq!(starts(&path), 0);
        // A dispatcher start with an empty field on a deck whose
        // `default_command` is `agy`, resolved and trimmed by the dialog.
        assert!(shows(&path, Some("agy")));
        // The same default with arguments, and a typed command with spaces
        // around it, which the dialog sends as typed.
        assert!(!shows(&path, Some("agy --model x")));
        assert!(!shows(&path, Some("  agy  ")));
        assert_eq!(starts(&path), 3, "each Antigravity start counts once");
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
        assert_eq!(
            record_choice(&mut session(&path), NoticeChoice::Dismiss),
            None
        );
        for start in 2..=300 {
            assert!(!tui_start(&path), "TUI start {start} showed");
        }
    }

    #[test]
    fn dont_show_again_in_the_tui_holds_in_the_desktop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notice.json");
        assert!(tui_start(&path));
        assert_eq!(session(&path).dismiss_permanently(), None);
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
            assert_eq!(record_choice(&mut session(&path), choice), None);
            for start in 2..=NOTICE_INTERVAL {
                assert!(!tui_start(&path), "{choice:?}: start {start} showed");
            }
            assert!(tui_start(&path), "{choice:?}: the notice did not come back");
        }
    }

    /// A Don't show again that cannot be written is reported once, not lost
    /// silently, and holds for the rest of the session as in the TUI. The
    /// state's directory is a file here, so nothing can be saved.
    #[test]
    fn a_dismissal_that_cannot_be_saved_is_said_once_and_holds() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("notice.json");
        let mut session = session(&path);
        let said = record_choice(&mut session, NoticeChoice::Dismiss);
        assert!(
            said.as_deref()
                .is_some_and(|why| why.contains("Antigravity notice")),
            "{said:?}"
        );
        assert_eq!(record_choice(&mut session, NoticeChoice::Later), None);
        assert_eq!(
            record_choice(&mut session, NoticeChoice::Dismiss),
            None,
            "said twice"
        );
        for start in 1..=150 {
            let next = record_start(&mut session, Some("agy"));
            assert_eq!(
                next,
                NoticeStart {
                    show: false,
                    problem: None
                },
                "start {start}"
            );
        }
    }

    /// A start that cannot be saved is said once, and kept in the session:
    /// the next start does not show the notice again, and the hundredth after
    /// the showing does.
    #[test]
    fn a_start_that_cannot_be_saved_is_said_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let mut session = session(&blocker.join("notice.json"));
        let first = record_start(&mut session, Some("agy"));
        assert!(first.show);
        assert!(first.problem.is_some(), "the problem was not said");
        for start in 2..=NOTICE_INTERVAL {
            assert_eq!(
                record_start(&mut session, Some("agy")),
                NoticeStart {
                    show: false,
                    problem: None
                },
                "start {start}"
            );
        }
        assert!(record_start(&mut session, Some("agy")).show);
    }

    /// The webview reads `show`, and `problem` only when there is one.
    #[test]
    fn a_start_answers_the_webview_show_and_any_problem() {
        let quiet = NoticeStart {
            show: true,
            problem: None,
        };
        assert_eq!(
            serde_json::to_value(quiet).unwrap(),
            serde_json::json!({ "show": true })
        );
        let said = NoticeStart {
            show: false,
            problem: Some("why".to_string()),
        };
        assert_eq!(
            serde_json::to_value(said).unwrap(),
            serde_json::json!({ "show": false, "problem": "why" })
        );
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
