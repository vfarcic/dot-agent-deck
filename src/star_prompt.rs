//! The star-repo reminder's words (PRD #37), shared by the TUI's prompt and
//! the desktop app's dialog (PRD #2174, CLAUDE.md rule 22).
//!
//! The reminder's state is [`crate::config::StarPromptState`]; this module
//! holds only what the two clients say, so a test in each can hold its own
//! copy of the words to this one. The desktop's copy is
//! `desktop/src/lib/starPrompt.ts`, which a Rust test in the desktop crate
//! reads.

use crate::repo_identity;

/// The repository page Star opens.
pub const REPO_URL: &str = repo_identity::URL;

/// The repository as the prompt shows it, without the scheme.
pub const REPO_DISPLAY: &str = repo_identity::DISPLAY;

/// The prompt's title, without the TUI border's padding and star.
pub const TITLE: &str = "Enjoying dot-agent-deck?";

/// The two lines of the invitation.
pub const BODY: [&str; 2] = [
    "If you find dot-agent-deck useful,",
    "please consider starring the repo!",
];

/// Star: open the repository page and stop asking (`s`).
pub const STAR: &str = "Star";
/// Later: ask again after another ten launches (`l`, or `Esc`).
pub const LATER: &str = "Later";
/// Don't ask again: stop asking (`d`).
pub const DONT_ASK_AGAIN: &str = "Don't ask again";

/// The status message for a star attempt: thanks when the browser opened,
/// and where to go when it did not.
pub fn star_message(opened: bool) -> String {
    if opened {
        "Thanks for starring! ⭐".to_string()
    } else {
        format!("Visit {} to star ⭐", repo_identity::DISPLAY)
    }
}
