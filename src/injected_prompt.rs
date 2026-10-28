//! Whether a hook-reported prompt is a turn the agent's harness submitted
//! itself rather than one a person typed (issue #1993).
//!
//! Claude Code reports some turns it adds on its own through the same
//! `UserPromptSubmit` hook a typed prompt uses: when a background command or
//! subagent finishes it submits a `<task-notification>` turn. Its payload has
//! no field saying who wrote the prompt (the schema's `source` is not sent by
//! Claude Code 2.1.312), so the turn is recognised by the wrapper it opens with.
//!
//! The card's prompt — `last_user_prompt`, `first_prompts` and the TUI card's
//! prompt lines — skips such a turn and keeps the previous typed prompt. The
//! hook's `user_prompt` itself is left as reported:
//! [`crate::submitted_prompt`] reads it to confirm that a prompt the deck typed
//! was delivered.
//!
//! The list is kept to wrappers seen arriving through a prompt hook. A slash
//! command the user typed is a typed prompt and matches none of them. A deck
//! message is labelled before it gets here ([`crate::inbox_prompt`]), so its
//! `✉` label matches none of them either.
//!
//! What was found for the other agents (CLAUDE.md rule 20):
//!
//! - OpenCode marks the text it adds to a user message itself `synthetic`; the
//!   deck's plugin does not report it ([`crate::opencode_manage`]), so it never
//!   reaches this check.
//! - Pi starts its own runs from `custom` messages, which carry no prompt the
//!   deck's extension could report.
//! - Codex (`<subagent_notification>`) and Devin
//!   (`<subagent_completion_notification>`) have notification wrappers of their
//!   own, but nothing has shown either one arriving through `UserPromptSubmit`,
//!   so neither is listed.

/// The openings of the turns a harness submits through the prompt hook.
///
/// Matched against the start of the prompt only: the hook cuts a prompt to
/// [`crate::submitted_prompt::USER_PROMPT_MAX_LEN`] bytes, so a notification's
/// closing tag is usually gone by the time it is reported.
const HARNESS_TURN_OPENERS: &[&str] = &["<task-notification>"];

/// Whether `prompt` is a turn the agent's harness submitted, not one a person
/// typed.
///
/// The opener must be followed by the notification's first child tag, as every
/// such turn is, so a person's prompt that merely starts with the tag's text
/// is still theirs.
pub fn is_harness_turn(prompt: &str) -> bool {
    let prompt = prompt.trim_start();
    HARNESS_TURN_OPENERS.iter().any(|opener| {
        prompt
            .strip_prefix(opener)
            .is_some_and(|rest| rest.trim_start().starts_with('<'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_notification_is_a_harness_turn() {
        assert!(is_harness_turn(
            "<task-notification><task-id>byn96iho8</task-id><tool-use-id>toolu_01</tool-use-id>"
        ));
        assert!(is_harness_turn(
            "\n<task-notification>\n<summary>Background command completed</summary>"
        ));
    }

    #[test]
    fn a_typed_prompt_is_not_a_harness_turn() {
        for typed in [
            "fix the login bug",
            "/review 42",
            "✉ build finished",
            "explain <task-notification> to me",
            "<task-notification> what does this tag mean?",
            "<task-notification>",
            "<task-notifications> are noisy",
            "",
        ] {
            assert!(!is_harness_turn(typed), "{typed:?}");
        }
    }
}
