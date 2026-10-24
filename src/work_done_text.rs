//! PRD #1666 part 3, round 5 (plan D5, D10): what a worker reads after
//! `work-done`. The daemon writes a refusal's sentence into its reply; the
//! CLI writes everything else from the reply's fields. Both read this one
//! copy, so a wording changes in one place.

use crate::event::{WorkDoneDisposition, WorkDoneTaskReply};
use crate::report_recovery::Destination;

/// The stable codes a refusal carries (plan D5).
pub mod code {
    pub const UNKNOWN_TASK: &str = "unknown_task";
    pub const TASK_SUPERSEDED: &str = "task_superseded";
    pub const TASK_CLOSED: &str = "task_closed";
    pub const INCOMPATIBLE_MODE: &str = "incompatible_mode";
    pub const NOT_A_WORKER: &str = "not_a_worker";
    pub const DECK_UNAVAILABLE: &str = "deck_unavailable";
    pub const UNATTESTED: &str = "unattested";
    pub const REPORT_PENDING: &str = "report_pending";
    pub const REPORT_TOO_LARGE: &str = "report_too_large";
    pub const REPORT_STORE_FULL: &str = "report_store_full";
    pub const REPORT_SAVE_FAILED: &str = "report_save_failed";
    pub const DECK_FULL: &str = "deck_full";
    pub const UNSUPPORTED_MODE: &str = "unsupported_mode";
    /// Not a refusal on the task-scoped verb, which answers
    /// `already_accepted` with `differs`: the legacy verb's acknowledgement
    /// has no such field, so it refuses a differing repeat with this code.
    pub const REPORT_DIFFERS: &str = "report_differs";
}

const NOTHING: &str = "nothing was reported";

/// A task id the deck does not know.
pub fn unknown_task(task_id: &str, remembered: usize) -> String {
    format!(
        "{NOTHING}: the deck does not know task {task_id} for this agent. It may have been \
         reported already and forgotten (the deck remembers the last {remembered} tasks of an \
         orchestration until it restarts), or it was delegated to another worker; nothing was \
         reported twice."
    )
}

/// A dispatched unit the deck no longer remembers.
pub fn forgotten_unit() -> String {
    format!(
        "{NOTHING}: the deck no longer remembers this dispatched unit. It already reported, ended \
         or was abandoned, and its caller was told which; nothing was reported twice."
    )
}

/// A task a newer task replaced (`by`), or a restart of the worker (`None`).
pub fn task_superseded(task_id: &str, by: Option<&str>) -> String {
    match by {
        Some(by) => format!(
            "{NOTHING}: task {task_id} was replaced by task {by} (delegate --supersede) before \
             this report was taken. If your work answers the newer task, report it with \
             work-done --task-id {by}."
        ),
        None => format!(
            "{NOTHING}: task {task_id} was cancelled when this worker was restarted, before this \
             report was taken."
        ),
    }
}

/// A task whose worker generation ended before the report was taken.
pub fn task_closed(task_id: &str) -> String {
    format!(
        "{NOTHING}: task {task_id} ended before this report was taken, because the agent it was \
         delegated to exited or was closed, and its orchestrator was told."
    )
}

/// A dispatched unit that ended before its report was taken.
pub fn unit_ended() -> String {
    format!(
        "{NOTHING}: this dispatched unit already ended (it exited, was closed or could not be \
         restarted) before its report was taken, and its caller was told it ended without a \
         report."
    )
}

/// A unit terminal a respawn replaced before its report was taken.
pub fn replaced_generation() -> String {
    format!(
        "{NOTHING}: this agent was replaced by a restart of its pane before its report was taken. \
         The agent now in the pane reports for this unit."
    )
}

/// `--done` from an agent that is no dispatched unit's terminal.
pub fn not_a_terminal() -> String {
    format!(
        "{NOTHING}: this agent is not the last agent of a dispatched unit, so work-done --done \
         has nothing to complete. Report a delegated task with work-done --task-id <id>, or send \
         a note to your orchestrator with work-done and neither option."
    )
}

/// An orchestrator whose instance's generations disagree on how it started.
pub fn conflicting_origin() -> String {
    format!(
        "{NOTHING}: the deck cannot tell whether this orchestration was dispatched, so it cannot \
         tell where this report goes. The daemon log names the orchestration."
    )
}

/// A note from an agent the orchestrator does not delegate to.
pub fn not_a_worker() -> String {
    format!(
        "{NOTHING}: this agent is not a worker of an orchestration, so there is no orchestrator \
         to report to."
    )
}

/// A note from a pane that now belongs to another orchestration instance.
pub fn pane_reused() -> String {
    format!("{NOTHING}: this agent's pane now belongs to another orchestration; nothing was sent.")
}

/// An orchestrator's note that completes no task.
pub fn orchestrator_note() -> String {
    format!(
        "{NOTHING}: this agent is the orchestrator. Report the orchestration complete with \
         work-done --done."
    )
}

/// The daemon's report state is torn by an internal error (plan D15).
pub fn deck_unavailable(dir: &str) -> String {
    format!(
        "{NOTHING}: the deck daemon hit an internal error and has stopped taking reports (its \
         log has the details). This attempt saved nothing new. The daemon does not recover from \
         this by itself: restart it to take reports again (dot-agent-deck daemon stop, then start \
         the deck; stopping the daemon also stops the agents it runs). Reports it already saved \
         stay in {dir}; after the restart they are kept there, but not delivered or listed. A \
         restart also loses any dispatch result the deck holds only in memory."
    )
}

/// A task or terminal report that no token attests.
pub fn unattested() -> String {
    format!(
        "{NOTHING}: the deck could not tie this report to the agent that sent it (no hook \
         capability token came with it), and a task or unit report needs that. Run work-done \
         with the dot-agent-deck the deck daemon runs."
    )
}

/// What `report_pending` says, by why it is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// An earlier attempt for the same task is still being saved.
    Task,
    /// An earlier attempt for the same unit is still being saved.
    Unit,
    /// The unit's dispatch is still being recorded.
    Registration,
    /// The unit's agent is being restarted.
    Replacement,
    /// This agent already has another report being saved.
    Generation,
}

pub fn report_pending(why: Pending) -> String {
    let what = match why {
        Pending::Task => "an earlier work-done for this task is still being saved",
        Pending::Unit => "an earlier work-done for this unit is still being saved",
        Pending::Registration => "the deck is still recording this unit's dispatch",
        Pending::Replacement => "the deck is still restarting this unit's agent",
        Pending::Generation => "another work-done from this agent is still being saved",
    };
    format!(
        "nothing new was reported: {what}. Run the same command again in a moment; it is \
         answered with that report's result."
    )
}

pub fn report_too_large(bytes: usize, limit: u64) -> String {
    format!(
        "{NOTHING}: the report is {bytes} bytes, over the {limit}-byte limit. Shorten it, or put \
         the details in a file and name the file in the report."
    )
}

pub fn store_full(dir: &str) -> String {
    format!("{NOTHING}: the deck cannot keep this report: its saved-results store is full ({dir}).")
}

pub fn store_files_full(max: u32, dir: &str) -> String {
    format!(
        "{NOTHING}: the deck cannot keep this report: its saved-results store holds the most \
         reports it keeps ({max} files, {dir})."
    )
}

pub fn save_failed(detail: &str) -> String {
    format!(
        "{NOTHING}: the deck could not save this report ({detail}). This attempt saved nothing \
         new; run the same command again once that is fixed."
    )
}

pub fn deck_full(error: &str) -> String {
    format!(
        "{error}, so it could not take this work-done report; {NOTHING}, so run work-done again \
         once the deck has room"
    )
}

pub fn unsupported_mode() -> String {
    format!(
        "{NOTHING}: this deck daemon does not know the kind of report this dot-agent-deck sent. \
         Run work-done with the dot-agent-deck the daemon runs."
    )
}

/// A repeat of task `task_id`'s report with a different body.
pub fn task_differs(task_id: &str) -> String {
    format!(
        "Task {task_id} was already reported; this different report was NOT delivered. To tell \
         the orchestrator more, run work-done without --task-id: that is delivered as a note \
         that completes no task."
    )
}

/// A repeat of a unit's terminal report with a different body.
pub fn unit_differs() -> String {
    "This unit already reported that it finished; this different report was NOT delivered, and \
     the first one stands."
        .to_string()
}

/// The CLI's line when the daemon did not confirm task-scoped completion.
pub const NOT_CONFIRMED: &str = "Error: this deck daemon did not confirm it supports task \
     work-done reports; nothing was reported. Restart the deck daemon on this release, or run \
     work-done with the dot-agent-deck the daemon runs.";

/// What the CLI says when the report was sent and its outcome is unknown.
pub fn unconfirmed(task_id: Option<&str>) -> String {
    match task_id {
        Some(_) => "The report may have been accepted; its result is unconfirmed. Run the same \
                    command again: a repeat of the same --task-id is answered with its first \
                    result while the deck still remembers the task, and is not reported twice."
            .to_string(),
        None => "The report may have been accepted; its result is unconfirmed. Check with the \
                 orchestrator before repeating: a repeat would be a second note."
            .to_string(),
    }
}

/// Where a reply's text goes, and the exit status the CLI uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliAnswer {
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub success: bool,
}

/// The CLI's answer for a reply to a report of `task_id` (`None` for a
/// terminal report or a note), `terminal` when it was `--done`.
pub fn cli_answer(reply: &WorkDoneTaskReply, task_id: Option<&str>, terminal: bool) -> CliAnswer {
    let ok = |line: String| CliAnswer {
        stdout: Some(line),
        stderr: None,
        success: true,
    };
    let err = |line: String| CliAnswer {
        stdout: None,
        stderr: Some(line),
        success: false,
    };
    let recovered = reply.destination == Some(Destination::Recovery);
    match reply.disposition {
        WorkDoneDisposition::Accepted => match (task_id, terminal) {
            (Some(id), _) if recovered => ok(format!(
                "Reported task {id}. Its orchestrator is no longer running, so the deck saved \
                 the report in Deck notices."
            )),
            (Some(id), _) => ok(format!("Reported task {id} to the orchestrator.")),
            (None, true) if reply.destination == Some(Destination::None) => {
                ok("Reported the orchestration complete.".to_string())
            }
            (None, true) if recovered => ok("Reported this unit complete. The agent that \
                 dispatched it is no longer running, so the deck saved the report in Deck \
                 notices."
                .to_string()),
            (None, _) => ok("Reported this unit complete to the agent that dispatched it.".into()),
        },
        WorkDoneDisposition::AlreadyAccepted => match (task_id, reply.differs) {
            (Some(id), false) => ok(format!(
                "Task {id} was already reported; nothing new was sent."
            )),
            (Some(id), true) => err(task_differs(id)),
            (None, false) => ok("This unit was already reported complete; nothing new was \
                 sent."
                .to_string()),
            (None, true) => err(unit_differs()),
        },
        WorkDoneDisposition::Uncorrelated if recovered => ok("Sent as a note that completes no \
             task. Its orchestrator is no longer running, so the deck saved it in Deck notices."
            .to_string()),
        WorkDoneDisposition::Uncorrelated => ok("Sent to the orchestrator as a note that \
             completes no task. If you finished a delegated task, run work-done again with \
             --task-id <id> from your task."
            .to_string()),
        WorkDoneDisposition::Refused => {
            let code = reply.code.as_deref().unwrap_or("refused");
            err(format!(
                "Error: {} [{code}]",
                reply.message.clone().unwrap_or_else(|| NOTHING.to_string())
            ))
        }
        WorkDoneDisposition::Unknown => err(unconfirmed(task_id)),
    }
}
