#![cfg(all(feature = "e2e", unix))]

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use spec::spec;

/// Scenario: Run the real work-done CLI from an uncommissioned synthetic worker. One bound Untrusted report is fetched and rendered in the orchestrator pane, labelled unsolicited, with no commissioned summary file.
#[spec("orchestration/work-done/004")]
#[test]
fn work_done_004_unsolicited_completion_is_visibly_labelled_in_the_attached_tui() {
    route::worker_report(4);
}

/// Scenario: Send an unsolicited worker report larger than the inline budget through the real CLI. The orchestrator fetches its framed head and saved-copy path from the inbox; that saved file holds the entire Untrusted-fenced report.
#[spec("orchestration/work-done/009")]
#[test]
fn work_done_009_a_cut_unsolicited_report_names_its_saved_full_copy_in_the_attached_tui() {
    route::worker_report(9);
}

/// Scenario: Delegate a task and pass a worker-owned summary file to the real work-done CLI. The source survives byte for byte, a separately framed report copy is saved, and one bound report is fetched and rendered in the orchestrator pane.
#[spec("orchestration/work-done/012")]
#[test]
fn work_done_012_task_file_at_the_summary_path_survives_in_the_attached_tui() {
    route::worker_report(12);
}
