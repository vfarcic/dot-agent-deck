#![cfg(all(feature = "e2e", unix))]

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use spec::spec;

/// Scenario: Dispatch an isolated orchestration from an attached synthetic caller and complete it through the real CLI. One bound acknowledgement and one Untrusted-framed completion reach that caller through its inbox; a repeat completion posts nothing.
#[spec("dispatch/return/001")]
#[test]
fn dispatch_return_001_orchestration_completion_reaches_the_caller() {
    route::dispatch_return(1);
}

/// Scenario: Dispatch an orchestration, fetch its acknowledgement, then detach and reattach the caller in the real TUI. Its eventual completion must still be fetched and visibly rendered in the same caller pane.
#[spec("dispatch/return/002")]
#[test]
fn dispatch_return_002_callback_survives_caller_detach_and_reattach() {
    route::dispatch_return(2);
}

/// Scenario: Dispatch a single stand-in and send an unrelated pane completion before the unit completes; that pane's `work-done --done` is refused with `incompatible_mode`. Only the unit acknowledgement and completion are bound to the caller inbox, and its fetched report appears in the attached pane.
#[spec("dispatch/return/003")]
#[test]
fn dispatch_return_003_single_completion_routes_while_unknown_pane_stays_inert() {
    route::dispatch_return(3);
}

/// Scenario: Complete a dispatched single stand-in with an over-budget report. Its framed inbox completion names a full Untrusted-fenced saved copy under the unit worktree, while the fetched head and completion are visibly rendered in the caller pane.
#[spec("dispatch/return/008")]
#[test]
fn dispatch_return_008_cut_report_names_its_saved_full_copy_in_the_unit_worktree() {
    route::dispatch_return(8);
}
