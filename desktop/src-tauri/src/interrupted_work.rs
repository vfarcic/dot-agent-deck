//! PRD #1790 stage 2: the desktop's interrupted work — the work a deck
//! restart left behind, as each deck's daemon derives it from its work
//! journal. The Tauri commands in `lib.rs` resolve the deck, require a
//! compatible link and call these with its client; each is per deck and
//! pinned to the daemon process the webview named (`instance`), since an
//! item's id belongs to one daemon's state directory.
//!
//! The shared client withholds every request from a daemon that does not
//! advertise `interrupted-work`, or is not `instance`
//! ([`DaemonClient::list_interrupted_work`] and its siblings); those answer
//! [`InterruptedWorkReply::Unsupported`], which the webview shows as nothing.
//!
//! The desktop is the person's client: reclaim and dismiss carry no pane
//! claim (decision D4), as the TUI's do.

use std::future::Future;
use std::time::Duration;

use dot_agent_deck::daemon_client::{ClientError, DaemonClient, GatedQuery};
use dot_agent_deck::interrupted_work::{
    DismissOutcome, InterruptedList, InterruptedShow, ReclaimOutcome,
};

use crate::dto::InterruptedWorkReply;

/// The bound on a list. With the Git facts asked for, the daemon bounds
/// them at 20 seconds; the TUI waits the same.
pub(crate) const LIST_TIMEOUT: Duration = Duration::from_secs(40);
/// The bound on a show (an item's kept reports).
pub(crate) const SHOW_TIMEOUT: Duration = Duration::from_secs(15);
/// The bound on a reclaim, its dry run included: the daemon bounds its
/// checks and the removal each.
pub(crate) const RECLAIM_TIMEOUT: Duration = Duration::from_secs(120);
/// The bound on a dismissal.
pub(crate) const DISMISS_TIMEOUT: Duration = Duration::from_secs(15);

/// `request`'s answer within `timeout`, as the webview reads it.
async fn reply<T>(
    what: &str,
    timeout: Duration,
    instance: &str,
    request: impl Future<Output = Result<GatedQuery<T>, ClientError>>,
) -> Result<InterruptedWorkReply<T>, String> {
    match crate::daemon_bridge::bounded_reply_within(what, timeout, request).await? {
        GatedQuery::Answered(answer) => Ok(InterruptedWorkReply::Answered {
            instance: instance.to_string(),
            answer,
        }),
        GatedQuery::Unsupported => Ok(InterruptedWorkReply::Unsupported),
    }
}

/// The deck's interrupted work, with each item's Git and pull request facts
/// when `git` is set.
pub(crate) async fn list(
    client: &DaemonClient,
    instance: &str,
    git: bool,
) -> Result<InterruptedWorkReply<InterruptedList>, String> {
    reply(
        "ListInterruptedWork",
        LIST_TIMEOUT,
        instance,
        client.list_interrupted_work(git, Some(instance)),
    )
    .await
}

/// One item and a page of its kept reports' text, from its `reports_from`-th
/// on (the first page when `None`), or with `report` that one report — so a
/// report past the ones the list carries can be read.
pub(crate) async fn show(
    client: &DaemonClient,
    instance: &str,
    id: &str,
    report: Option<&str>,
    reports_from: Option<usize>,
) -> Result<InterruptedWorkReply<InterruptedShow>, String> {
    reply(
        "ShowInterrupted",
        SHOW_TIMEOUT,
        instance,
        client.show_interrupted_page(id, report, reports_from, Some(instance)),
    )
    .await
}

/// Reclaim an item — remove its worktree, keep its branch — or with
/// `dry_run` ask what that would do. No pane claim: the person's client.
pub(crate) async fn reclaim(
    client: &DaemonClient,
    instance: &str,
    id: &str,
    dry_run: bool,
) -> Result<InterruptedWorkReply<ReclaimOutcome>, String> {
    reply(
        "ReclaimInterrupted",
        RECLAIM_TIMEOUT,
        instance,
        client.reclaim_interrupted(id, dry_run, None, Some(instance)),
    )
    .await
}

/// Dismiss exactly the items `ids` name — never "all", so an item listed
/// after the person was asked is not dismissed unseen. No pane claim.
pub(crate) async fn dismiss(
    client: &DaemonClient,
    instance: &str,
    ids: Vec<String>,
) -> Result<InterruptedWorkReply<DismissOutcome>, String> {
    reply(
        "DismissInterrupted",
        DISMISS_TIMEOUT,
        instance,
        client.dismiss_interrupted(ids, false, None, Some(instance)),
    )
    .await
}

#[cfg(all(test, unix))]
#[path = "interrupted_work_tests.rs"]
mod tests;
