//! PRD #1790 stage 1: the task a dispatched unit was given, saved beside its
//! worktree so a later stage can hand it back after a daemon restart.
//!
//! The daemon writes `<worktree>/.dot-agent-deck/dispatch-task-<work id>.md`
//! when it dispatches a unit, for `dot-agent-deck dispatch`
//! ([`crate::dispatch::handle_dispatch`]) and for scheduled issue dispatch
//! ([`crate::issue_dispatch_run`]), holding the task text exactly as the
//! dispatcher gave it (for an issue, the prompt with its issue number
//! substituted). The work id is the unit's work in the journal, so a reader
//! joins the file to the journal by name. The text itself never goes into
//! the journal.
//!
//! - **Ignored by git without touching tracked files.** Before the file is
//!   written, `.dot-agent-deck/` is put in the clone's own
//!   `.git/info/exclude` ([`crate::orchestrator_context::ensure_git_excludes_context_dir`],
//!   issue #329), which a linked worktree shares; a worktree that is not a git
//!   checkout gets no file, since nothing would keep it out of a commit.
//! - **Not swept.** The coordination sweep's age bound
//!   ([`crate::orchestrator_context::is_sweepable_coordination_name`]) and
//!   the inbox spill retention ([`crate::inbox_spills`], which removes only
//!   the `inbox-message-*` files it minted) both leave it alone. It goes with
//!   the worktree.
//! - **Never overwritten.** It is created with `O_EXCL` through
//!   [`crate::orchestrator_context::write_new_coordination_file`], owner-only.

use std::io;
use std::path::{Path, PathBuf};

use crate::work_journal::WorkId;

/// The prefix of every saved task's file name.
const PREFIX: &str = "dispatch-task-";

/// The file name of `work`'s saved task: `dispatch-task-<work id>.md`.
pub fn file_name(work: &WorkId) -> String {
    format!("{PREFIX}{work}.md")
}

/// `name` is named as a saved dispatch task ([`file_name`]).
pub fn is_task_file_name(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(".md"))
        .is_some_and(|work| WorkId::parse(work).is_some())
}

/// Save `task` as `work`'s dispatched task in `worktree`, after making sure
/// git ignores it there. Answers the file's path.
pub fn save(worktree: &Path, work: &WorkId, task: &str) -> io::Result<PathBuf> {
    use crate::orchestrator_context::{GitExcludeOutcome, ensure_git_excludes_context_dir};
    if ensure_git_excludes_context_dir(worktree)? == GitExcludeOutcome::NotAGitRepo {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the worktree is not a git checkout, so the task file could not be kept out of a commit",
        ));
    }
    crate::orchestrator_context::write_new_coordination_file(worktree, &file_name(work), task)
}

/// [`save`], logging a failure instead of answering it: a dispatch whose task
/// could not be saved still runs, it just cannot be handed its task back
/// after a restart.
pub fn save_or_warn(worktree: &Path, work: Option<&WorkId>, task: &str) {
    let Some(work) = work else {
        return;
    };
    if let Err(error) = save(worktree, work, task) {
        tracing::warn!(
            worktree = %worktree.display(),
            %work,
            %error,
            "dispatch: could not save the dispatched task beside its worktree"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck_identity::DeckId;

    fn work() -> WorkId {
        WorkId::mint(&DeckId::parse("abcdefgh").unwrap()).unwrap()
    }

    #[test]
    fn its_name_is_recognised_and_nothing_else_is() {
        let w = work();
        let name = file_name(&w);
        assert!(name.starts_with("dispatch-task-w-abcdefgh-"));
        assert!(is_task_file_name(&name));
        for other in [
            "dispatch-task-.md",
            "dispatch-task-not-a-work.md",
            "worker-task-coder.md",
            "dispatch-task-w-abcdefgh-0123456789abcdef.txt",
        ] {
            assert!(!is_task_file_name(other), "{other}");
        }
    }

    /// The two retention sweeps in the worktree's `.dot-agent-deck/` leave a
    /// saved task alone: the coordination sweep by name, and the inbox spill
    /// retention because it is not one of its files.
    #[test]
    fn neither_sweep_takes_a_saved_task() {
        let name = file_name(&work());
        assert!(!crate::orchestrator_context::is_sweepable_coordination_name(&name));
        assert!(!crate::inbox_spills::is_spill_file_name(&name));
        // Control: an ordinary coordination file is still swept.
        assert!(crate::orchestrator_context::is_sweepable_coordination_name(
            "worker-task-coder.md"
        ));
    }

    #[test]
    fn a_task_outside_a_git_checkout_is_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let error = save(dir.path(), &work(), "TASK").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!dir.path().join(".dot-agent-deck").exists());
    }
}
