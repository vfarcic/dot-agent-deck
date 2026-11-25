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
//! The desktop is the person's client: reclaim, dismiss, restore and start
//! over carry no pane claim (decision D4), as the TUI's do.
//!
//! PRD #1790 stage 3: restore and start-over are the daemon's — it starts the
//! agent, as it does for the TUI and the CLI, so every client restores the
//! same way. The shared client withholds both from a daemon that does not
//! also advertise `restore-agents`. Neither these nor the webview's
//! controls for them are behind the experimental flag (issue #2494).

use std::future::Future;
use std::time::Duration;

use dot_agent_deck::daemon_client::{ClientError, DaemonClient, GatedQuery};
use dot_agent_deck::interrupted_work::{
    DismissOutcome, DismissStopReason, DismissStopped, InterruptedList, InterruptedShow,
    ReclaimOutcome, RestoreMode, RestoreOutcome, StartOverOutcome, dismiss_batches,
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
/// PRD #1790 stage 3: the bound on a restore or a start-over, dry runs
/// included. The daemon answers once each agent confirmed its conversation
/// or was stopped; the TUI waits the same.
pub(crate) const RESTORE_TIMEOUT: Duration = Duration::from_secs(120);

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

/// PRD #1790 stage 3: restore the attempts `agents` of item `id` with their
/// conversations, posting `mode`'s restart note first, or with `dry_run` ask
/// what that would do. No pane claim: the person's client.
pub(crate) async fn restore(
    client: &DaemonClient,
    instance: &str,
    id: &str,
    agents: Vec<String>,
    mode: RestoreMode,
    dry_run: bool,
) -> Result<InterruptedWorkReply<RestoreOutcome>, String> {
    reply(
        "RestoreInterrupted",
        RESTORE_TIMEOUT,
        instance,
        client.restore_interrupted(id, agents, mode, dry_run, None, Some(instance)),
    )
    .await
}

/// PRD #1790 stage 3: start attempt `agent` of item `id` over in its folder,
/// without its conversation, or with `dry_run` ask what that would do. No
/// pane claim.
pub(crate) async fn start_over(
    client: &DaemonClient,
    instance: &str,
    id: &str,
    agent: &str,
    dry_run: bool,
) -> Result<InterruptedWorkReply<StartOverOutcome>, String> {
    reply(
        "StartOverInterrupted",
        RESTORE_TIMEOUT,
        instance,
        client.start_over_interrupted(id, agent, dry_run, None, Some(instance)),
    )
    .await
}

/// Dismiss exactly the items `ids` name — never "all", so an item listed
/// after the person was asked is not dismissed unseen. No pane claim.
///
/// The ids go in the batches the daemon admits
/// ([`dot_agent_deck::interrupted_work::dismiss_batches`]), one after
/// another, each pinned to `instance`, and the answer adds them up: one call
/// is the one dismissal the webview holds pending. A later batch that does
/// not dismiss — refused whole, withheld because the daemon process was
/// replaced, or unanswered — stops the rest, and the answer records it in
/// [`DismissOutcome::stopped`]. The first batch answers as a single
/// dismissal does.
pub(crate) async fn dismiss(
    client: &DaemonClient,
    instance: &str,
    ids: Vec<String>,
) -> Result<InterruptedWorkReply<DismissOutcome>, String> {
    let mut batches = dismiss_batches(&ids).into_iter();
    let first = batches.next().unwrap_or_default();
    let mut done = match reply(
        "DismissInterrupted",
        DISMISS_TIMEOUT,
        instance,
        client.dismiss_interrupted(first, false, None, Some(instance)),
    )
    .await?
    {
        InterruptedWorkReply::Answered { answer, .. } if answer.refusal.is_none() => answer,
        other => return Ok(other),
    };
    let rest: Vec<Vec<String>> = batches.collect();
    for (n, batch) in rest.iter().enumerate() {
        let answer = crate::daemon_bridge::bounded_reply_within(
            "DismissInterrupted",
            DISMISS_TIMEOUT,
            client.dismiss_interrupted(batch.clone(), false, None, Some(instance)),
        )
        .await;
        let (reason, refusal) = match answer {
            Ok(GatedQuery::Answered(outcome)) if outcome.refusal.is_none() => {
                done.absorb(outcome);
                continue;
            }
            Ok(GatedQuery::Answered(outcome)) => (
                DismissStopReason::Refused,
                outcome.refusal.map(|r| r.reason),
            ),
            Ok(GatedQuery::Unsupported) => (DismissStopReason::Replaced, None),
            Err(_) => (DismissStopReason::Failed, None),
        };
        done.stopped = Some(DismissStopped {
            reason,
            refusal,
            not_sent: rest[n..].iter().map(Vec::len).sum(),
        });
        break;
    }
    Ok(InterruptedWorkReply::Answered {
        instance: instance.to_string(),
        answer: done,
    })
}

#[cfg(all(test, unix))]
#[path = "interrupted_work_tests.rs"]
mod tests;
