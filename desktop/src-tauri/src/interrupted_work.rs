//! PRD #1790 stage 2: the desktop's interrupted work — not implemented yet
//! (the RED half of a TDD pair: its tests are in `interrupted_work_tests.rs`).
#![cfg_attr(not(test), allow(dead_code))]

use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::interrupted_work::{
    DismissOutcome, InterruptedList, InterruptedShow, ReclaimOutcome,
};

use crate::dto::InterruptedWorkReply;

const NOT_YET: &str = "interrupted work is not implemented yet";

pub(crate) async fn list(
    _client: &DaemonClient,
    _instance: &str,
    _git: bool,
) -> Result<InterruptedWorkReply<InterruptedList>, String> {
    Err(NOT_YET.into())
}

pub(crate) async fn show(
    _client: &DaemonClient,
    _instance: &str,
    _id: &str,
    _report: Option<&str>,
    _reports_from: Option<usize>,
) -> Result<InterruptedWorkReply<InterruptedShow>, String> {
    Err(NOT_YET.into())
}

pub(crate) async fn reclaim(
    _client: &DaemonClient,
    _instance: &str,
    _id: &str,
    _dry_run: bool,
) -> Result<InterruptedWorkReply<ReclaimOutcome>, String> {
    Err(NOT_YET.into())
}

pub(crate) async fn dismiss(
    _client: &DaemonClient,
    _instance: &str,
    _ids: Vec<String>,
) -> Result<InterruptedWorkReply<DismissOutcome>, String> {
    Err(NOT_YET.into())
}

#[cfg(all(test, unix))]
#[path = "interrupted_work_tests.rs"]
mod tests;
