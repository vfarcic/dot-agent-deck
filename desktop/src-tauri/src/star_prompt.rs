//! PRD #2174: the desktop app's star-repo reminder, the TUI's star prompt
//! (PRD #37) in the desktop's own dialog.
//!
//! **One reminder state, shared with the TUI.** Both clients read and write
//! the same per-user file, `config::StarPromptState` at
//! `config::star_prompt_path` (`star-prompt-state.json` in the user's config
//! directory, or the file `DOT_AGENT_DECK_STAR_PROMPT` names). A launch of
//! either client counts towards the next reminder, and Star or Don't ask again
//! in either stops both. It is the user's own file on this machine, not a
//! deck's: the reminder is a client feature and moves nothing on any daemon,
//! so connecting to remote or several decks does not change how often it asks.
//!
//! **One launch per app process.** The webview asks at startup
//! ([`desktop_star_prompt_launch`]), and may ask again when it reloads (a dev
//! server's hot reload, React's development double-mount); only the first ask
//! counts a launch, and the answer is kept for the rest of the process until a
//! choice is made ([`desktop_star_prompt_choose`]).

use std::path::PathBuf;
use std::sync::Mutex;

use dot_agent_deck::config::{StarPromptState, star_prompt_path};
use dot_agent_deck::star_prompt::{REPO_URL, star_message};
use dot_agent_deck::system_browser::{LAUNCH_GRACE, open_with_platform_opener};
use serde::Deserialize;
use tauri::Webview;

/// The dialog's three choices, as the webview names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StarChoice {
    /// Star (`s`): open the repository page and stop asking.
    Star,
    /// Later (`l`, `Esc`): ask again after another ten launches.
    Later,
    /// Don't ask again (`d`): stop asking.
    Dismiss,
}

/// This process's launch: counted on the first ask, remembered after it.
pub struct StarLaunch {
    due: Mutex<Option<bool>>,
    /// Whether a choice has been saved in this process.
    chosen: std::sync::atomic::AtomicBool,
}

impl StarLaunch {
    pub const fn new() -> Self {
        Self {
            due: Mutex::new(None),
            chosen: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether to show the reminder at this launch. The first call counts the
    /// launch in the file at `path`; every later call answers the same until
    /// [`Self::answered`].
    pub fn check(&self, path: PathBuf) -> bool {
        let mut due = self.due.lock().unwrap_or_else(|e| e.into_inner());
        *due.get_or_insert_with(|| StarPromptState::load_from(path).increment_and_check())
    }

    /// A choice was saved: this process shows no reminder again. Answers
    /// whether it is the process's first, so that only one Star opens the
    /// browser however many requests overlap (Qodo, PR #2321).
    pub fn answered(&self) -> bool {
        *self.due.lock().unwrap_or_else(|e| e.into_inner()) = Some(false);
        !self.chosen.swap(true, std::sync::atomic::Ordering::SeqCst)
    }
}

static STAR_LAUNCH: StarLaunch = StarLaunch::new();

/// Record `choice` in the file at `path`, over what the file holds now, so a
/// choice the TUI saved meanwhile is kept. The error says why it could not be
/// saved.
pub fn record_choice(path: PathBuf, choice: StarChoice) -> Result<(), String> {
    let mut state = StarPromptState::load_from(path);
    match choice {
        StarChoice::Star | StarChoice::Dismiss => state.dismiss_permanently(),
        StarChoice::Later => state.snooze(),
    }
}

/// Whether to show the star reminder at this launch of the app.
#[tauri::command]
pub async fn desktop_star_prompt_launch(webview: Webview) -> Result<bool, String> {
    crate::ensure_main_webview(&webview)?;
    let path = star_prompt_path();
    tokio::task::spawn_blocking(move || STAR_LAUNCH.check(path))
        .await
        .map_err(|error| format!("Could not read the star reminder's state: {error}"))
}

/// The user's choice in the star reminder. Answers the sentence to show for
/// Star — thanks, or where to go when the browser did not open — and nothing
/// for the other two, which the TUI does not report either. A choice that
/// could not be saved is an error, and leaves this process's reminder due, so
/// the webview can show it again; Star opens no browser then.
#[tauri::command]
pub async fn desktop_star_prompt_choose(
    webview: Webview,
    choice: StarChoice,
) -> Result<Option<String>, String> {
    crate::ensure_main_webview(&webview)?;
    let path = star_prompt_path();
    tokio::task::spawn_blocking(move || {
        record_choice(path, choice)?;
        let first = STAR_LAUNCH.answered();
        Ok((first && choice == StarChoice::Star)
            .then(|| star_message(open_with_platform_opener(REPO_URL, LAUNCH_GRACE).is_ok())))
    })
    .await
    .map_err(|error| format!("Could not save the star reminder's choice: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TUI's launch, as `ui.rs` makes it: read the file, count, answer.
    fn tui_launch(path: &std::path::Path) -> bool {
        StarPromptState::load_from(path).increment_and_check()
    }

    /// A fresh desktop process's launch.
    fn desktop_launch(path: &std::path::Path) -> bool {
        StarLaunch::new().check(path.to_path_buf())
    }

    fn launches(path: &std::path::Path) -> u64 {
        StarPromptState::load_from(path).launch_count
    }

    #[test]
    fn a_desktop_process_counts_one_launch_however_often_it_asks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        let process = StarLaunch::new();
        for _ in 0..3 {
            assert!(!process.check(path.clone()));
        }
        assert_eq!(launches(&path), 1);
    }

    #[test]
    fn the_tenth_launch_asks_whichever_client_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        for launch in 1..=9 {
            let asked = if launch % 2 == 0 {
                tui_launch(&path)
            } else {
                desktop_launch(&path)
            };
            assert!(!asked, "launch {launch} asked");
        }
        let process = StarLaunch::new();
        assert!(process.check(path.clone()), "the tenth launch did not ask");
        assert!(process.check(path.clone()), "a reload forgot the reminder");
        assert!(process.answered(), "the first choice is the first");
        assert!(!process.answered(), "a second choice is not");
        assert!(!process.check(path.clone()), "a choice did not close it");
    }

    #[test]
    fn dont_ask_again_in_the_tui_stops_the_desktop_reminder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        for _ in 0..9 {
            tui_launch(&path);
        }
        let mut tui = StarPromptState::load_from(&path);
        assert!(tui.increment_and_check());
        tui.dismiss_permanently().unwrap();
        for launch in 11..=40 {
            assert!(!desktop_launch(&path), "desktop launch {launch} asked");
        }
    }

    #[test]
    fn a_choice_in_the_desktop_holds_in_the_tui() {
        for choice in [StarChoice::Star, StarChoice::Dismiss] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("star.json");
            for _ in 0..9 {
                tui_launch(&path);
            }
            assert!(desktop_launch(&path));
            record_choice(path.clone(), choice).unwrap();
            for launch in 11..=40 {
                assert!(!tui_launch(&path), "{choice:?}: TUI launch {launch} asked");
            }
        }
    }

    #[test]
    fn later_in_the_desktop_delays_the_tui_ten_launches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        for _ in 0..9 {
            tui_launch(&path);
        }
        assert!(desktop_launch(&path));
        record_choice(path.clone(), StarChoice::Later).unwrap();
        for launch in 11..=19 {
            assert!(!tui_launch(&path), "TUI launch {launch} asked");
        }
        assert!(tui_launch(&path), "TUI launch 20 did not ask");
    }

    #[test]
    fn later_in_the_desktop_keeps_a_dismissal_the_tui_saved_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("star.json");
        for _ in 0..9 {
            tui_launch(&path);
        }
        assert!(desktop_launch(&path));
        let mut tui = StarPromptState::load_from(&path);
        assert!(tui.increment_and_check());
        tui.dismiss_permanently().unwrap();
        record_choice(path.clone(), StarChoice::Later).unwrap();
        assert!(StarPromptState::load_from(&path).permanently_dismissed);
    }

    /// Qodo, PR #2321: a choice that cannot be written is reported, not lost
    /// silently. The state's directory is a file here, so nothing can be saved.
    #[test]
    fn a_choice_that_cannot_be_saved_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, "").unwrap();
        let path = blocker.join("star.json");
        for choice in [StarChoice::Star, StarChoice::Later, StarChoice::Dismiss] {
            assert!(record_choice(path.clone(), choice).is_err(), "{choice:?}");
        }
    }

    #[test]
    fn the_webview_names_the_three_choices() {
        for (name, choice) in [
            ("star", StarChoice::Star),
            ("later", StarChoice::Later),
            ("dismiss", StarChoice::Dismiss),
        ] {
            assert_eq!(
                serde_json::from_value::<StarChoice>(serde_json::json!(name)).unwrap(),
                choice
            );
        }
    }

    /// CLAUDE.md rule 22: the dialog says what the TUI's prompt says.
    #[test]
    fn the_dialog_uses_the_tui_prompts_words() {
        use dot_agent_deck::star_prompt::{BODY, DONT_ASK_AGAIN, LATER, REPO_DISPLAY, STAR, TITLE};
        let words = include_str!("../../src/lib/starPrompt.ts");
        for (what, word) in [
            ("title", TITLE),
            ("first line", BODY[0]),
            ("second line", BODY[1]),
            ("star", STAR),
            ("later", LATER),
            ("don't ask again", DONT_ASK_AGAIN),
            ("repository", REPO_DISPLAY),
            ("repository URL", REPO_URL),
        ] {
            assert!(
                words.contains(&format!("\"{word}\"")),
                "starPrompt.ts must carry the TUI's {what} word {word:?}"
            );
        }
        for opened in [true, false] {
            let message = star_message(opened);
            assert!(
                words.contains(&format!("\"{message}\"")),
                "starPrompt.ts must carry the TUI's star message {message:?}"
            );
        }
    }
}
