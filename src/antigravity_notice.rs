//! The Antigravity experimental notice's words (PRD #211), shared by the
//! TUI's dialog and the desktop app's (CLAUDE.md rule 22).
//!
//! When a user starts Antigravity from either client's New Agent flow, a
//! dialog says that Antigravity support is experimental and asks for an
//! issue: a problem, or word that it works. It shows on the first such start
//! and then on every [`NOTICE_INTERVAL`]th start after the last showing, until
//! the user says Don't show again.
//!
//! The pattern is the star-repo reminder's ([`crate::star_prompt`]): this
//! module holds only what the two clients say, and the state is
//! [`crate::config::AntigravityNoticeState`], one per-user file both clients
//! read and write. The desktop's copy of the words is
//! `desktop/src/lib/antigravityNotice.ts`, which a Rust test in the desktop
//! crate reads.

use crate::repo_identity;

/// Starts between two showings: the dialog shows on the first counted start,
/// then again on the hundredth counted start after the last showing.
pub const NOTICE_INTERVAL: u64 = 100;

/// The dialog's title.
pub const TITLE: &str = "Antigravity support is experimental";

/// The request, followed in the dialog by [`issues_display`].
pub const BODY: &str =
    "Tell us how it works for you: report a problem, or let us know it works as expected.";

/// Open issues: open the repository's new-issue page (`o`).
pub const OPEN_ISSUES: &str = "Open issues";
/// Later: show again at the next interval (`l`, or `Esc`).
pub const LATER: &str = "Later";
/// Don't show again: never show it again, in either client (`d`).
pub const DONT_SHOW_AGAIN: &str = "Don't show again";

/// The repository's issues, as the dialog shows them, without the scheme
/// (`github.com/vfarcic/dot-agent-deck/issues` upstream).
pub fn issues_display() -> String {
    format!("{}/issues", repo_identity::DISPLAY)
}

/// The page Open issues opens: the repository's new-issue page.
pub fn new_issue_url() -> String {
    format!("{}/issues/new", repo_identity::URL)
}

/// What an Open issues attempt reports: thanks when the browser opened, and
/// the address to go to when it did not, as the star reminder's
/// [`crate::star_prompt::star_message`] does.
pub fn open_issues_message(opened: bool) -> String {
    if opened {
        "Thanks for trying Antigravity in dot-agent-deck!".to_string()
    } else {
        format!(
            "Visit {} to tell us how Antigravity works",
            issues_display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_issues_address_is_built_from_the_repo_identity() {
        assert_eq!(
            issues_display(),
            format!("{}/issues", repo_identity::DISPLAY)
        );
        assert_eq!(
            new_issue_url(),
            format!("{}/issues/new", repo_identity::URL)
        );
    }

    #[test]
    fn a_browser_that_did_not_open_is_told_where_to_go() {
        assert!(open_issues_message(false).contains(&issues_display()));
        assert!(!open_issues_message(true).contains("Visit"));
    }
}
