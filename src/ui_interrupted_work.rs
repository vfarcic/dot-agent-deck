//! PRD #1790 stage 2: the TUI's interrupted work — the work a deck restart
//! left behind, as the daemon derives it from its work journal.
//!
//! - **Fetch.** Once per daemon process the TUI's event stream is attached to
//!   ([`sync`]): after the TUI attaches, and again after it reconnects to
//!   another process. The request runs on the tokio runtime, never on the
//!   render thread, and its answer is taken once per frame ([`drain`]).
//!   [`DaemonClient::list_interrupted_work`] withholds it from a daemon that
//!   does not advertise `interrupted-work`; an unsupported or failed fetch
//!   shows nothing. Nothing polls on a timer: the list is asked for again only
//!   when it is opened and after a reclaim or dismissal.
//! - **Footer.** While the list has items, a footer row says how many
//!   ([`crate::interrupted_work::interrupted_footer`]) with `[Show]` and
//!   `[Hide]` buttons; a list that came back empty but incomplete gets the
//!   row too, saying some work may not be listed
//!   ([`crate::interrupted_work::INTERRUPTED_UNREAD`]).
//! - **Overlay** (`UiMode::InterruptedWork`): the list, an item's details, and
//!   the reclaim and dismiss-all confirmations. In the details, `Enter` shows
//!   the first page of the item's kept reports with their text, and `n`/`p`
//!   the next and previous page, so a report past the ones the list carries
//!   can be reached; only the page last asked for is shown.
//! - **Answers.** Each answer carries the daemon process it came from and
//!   the request it answers ([`Tag`]); one from a process the TUI has since
//!   left, or for a request no longer waited for, is dropped before it
//!   changes anything. Backing out of a dry run abandons it; a reclaim or
//!   dismissal in flight keeps the actions waiting until it answers.
//!
//! Every sentence shown is one of the shared wording constants and helpers of
//! [`crate::interrupted_work`] (decision D2), so the CLI, the TUI and the
//! desktop say the same thing. Reclaim and dismiss are sent with no pane claim:
//! the TUI is the person's client (decision D4).

use std::future::Future;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::daemon_client::{ClientError, DaemonClient, GatedQuery, Incarnation};
use crate::interrupted_work::{
    self as words, DismissOutcome, GitFacts, InterruptedItem, InterruptedList, InterruptedShow,
    ItemKind, ReclaimOutcome, ReclaimVerdict,
};

/// The bound on a list request. A list opened from the overlay asks for the
/// Git and pull request facts, which the daemon bounds at 20 seconds.
const LIST_TIMEOUT: Duration = Duration::from_secs(40);
/// The bound on a show request (an item's kept reports).
const SHOW_TIMEOUT: Duration = Duration::from_secs(15);
/// The bound on a reclaim, its dry run included: its checks and the removal
/// are each bounded by the daemon.
const RECLAIM_TIMEOUT: Duration = Duration::from_secs(120);
/// The bound on a dismissal.
const DISMISS_TIMEOUT: Duration = Duration::from_secs(15);
/// The overlay's widest.
const POPUP_MAX_WIDTH: u16 = 100;

/// What a key or a click asks of the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    Up,
    Down,
    PageUp,
    PageDown,
    /// `Enter`: open the selected item's details; in details, show its kept
    /// reports' text.
    Open,
    /// `n` in details: the next page of the item's kept reports.
    NextReports,
    /// `p` in details: the page before.
    PreviousReports,
    Reclaim,
    Dismiss,
    DismissAll,
    /// `y` in a confirmation.
    Yes,
    /// Back to the list from details or a confirmation.
    Back,
    Close,
}

/// What the overlay shows.
#[derive(Debug, Clone, Default)]
pub(super) enum View {
    #[default]
    List,
    /// One item's details. `asked`: its kept reports' text was asked for and
    /// has not failed. `page`: where the page of kept reports last asked for
    /// starts. `failed`: why the last report request failed.
    Details {
        id: String,
        show: Option<Box<InterruptedShow>>,
        asked: bool,
        scroll: usize,
        page: Option<usize>,
        failed: Option<String>,
    },
    /// Reclaim's confirmation: the dry run's outcome once it answered.
    Reclaim {
        id: String,
        dry: Option<ReclaimOutcome>,
        scroll: usize,
    },
    /// Dismiss all's confirmation, naming exactly the items listed when it was
    /// asked, so one that appears meanwhile is not dismissed unseen.
    DismissAll { ids: Vec<String>, scroll: usize },
}

/// Which daemon process and which request an answer belongs to. An answer is
/// applied only while both are still the ones waited for: the process
/// (`generation`, which changes with every process the TUI attaches to) and
/// the request (`op`, a number no other request of this TUI shares).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tag {
    generation: u64,
    op: u64,
}

/// A daemon answer, delivered to the render thread.
pub(super) enum Answer {
    /// `Ok(None)`: the daemon does not support interrupted work.
    Listed {
        tag: Tag,
        result: Result<Option<InterruptedList>, String>,
    },
    Shown {
        tag: Tag,
        id: String,
        result: Result<Option<InterruptedShow>, String>,
    },
    Reclaimed {
        tag: Tag,
        id: String,
        dry_run: bool,
        result: Result<Option<ReclaimOutcome>, String>,
    },
    Dismissed {
        tag: Tag,
        result: Result<Option<DismissOutcome>, String>,
    },
}

impl Answer {
    fn tag(&self) -> Tag {
        match self {
            Answer::Listed { tag, .. }
            | Answer::Shown { tag, .. }
            | Answer::Reclaimed { tag, .. }
            | Answer::Dismissed { tag, .. } => *tag,
        }
    }
}

/// A reclaim or dismissal in flight: the actions wait for its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    op: u64,
    /// A dry run only asks; backing out of its confirmation abandons it, and
    /// its late answer is dropped.
    dry_run: bool,
}

/// A dismissal's batches: one is sent at a time, all pinned to the daemon
/// process the list came from, and the next only once the one before answered
/// with a dismissal (final review, reviewer 1 / auditor 2).
#[derive(Debug, Default)]
struct Batches {
    /// The ids of the batch in flight.
    in_flight: usize,
    /// The batches not yet sent, in order.
    left: std::collections::VecDeque<Vec<String>>,
    /// What the answered batches did, added up; `None` before the first
    /// answered.
    done: Option<DismissOutcome>,
}

impl Batches {
    /// How many ids the batch in flight and the ones after it name.
    fn not_sent(&self) -> usize {
        self.in_flight + self.left.iter().map(Vec::len).sum::<usize>()
    }
}

/// What the TUI holds of the interrupted work list.
pub(super) struct InterruptedWork {
    /// The daemon process the list came from; every request is pinned to it,
    /// since an item's id belongs to one daemon's state directory.
    instance: Option<String>,
    /// The daemon process the attach fetch was made for.
    fetched_for: Option<String>,
    /// Counts the daemon processes the TUI attached to; an answer tagged
    /// with an earlier one is dropped.
    generation: u64,
    /// The last request number given out, across processes.
    last_op: u64,
    /// The last list the daemon answered; `None` before an answer, and after
    /// one that was unsupported or failed.
    list: Option<InterruptedList>,
    /// Why there is no list, for the overlay.
    unavailable: Option<String>,
    /// The selected item, an index into [`ordered`].
    selected: usize,
    /// The list view's first row.
    scroll: usize,
    view: View,
    /// The last action's sentence.
    message: Option<String>,
    /// The footer row was hidden (`X`), until another daemon process.
    footer_hidden: bool,
    /// The reclaim or dismissal in flight.
    pending: Option<Pending>,
    /// A dismissal sent in batches ([`words::dismiss_batches`]): the batches
    /// not yet sent and what the answered ones did. Its batch in flight is
    /// [`Self::pending`].
    dismissal: Option<Batches>,
    /// The newest list request; an older one's answer is not applied.
    list_op: Option<u64>,
    /// The report request the open details wait for.
    show_op: Option<u64>,
    /// The rows of the y/n question as last drawn, each with whether a
    /// flushed frame has shown it whole since the question appeared: `y`
    /// counts only once every row has been seen, though not necessarily all
    /// in one frame, so a question taller than the popup can be scrolled
    /// through and then answered. Re-wrapped rows (a resize) start unseen.
    question_seen: Vec<(String, bool)>,
    /// The L1 seam's record of every request sent, in order; `None` in
    /// production, where each is sent to the daemon instead.
    recorder: Option<Vec<Recorded>>,
    tx: mpsc::Sender<Answer>,
    rx: mpsc::Receiver<Answer>,
}

/// A request the L1 seam recorded, with the tag its answer carries.
#[derive(Debug, Clone)]
pub(super) struct Recorded {
    tag: Tag,
    sent: InterruptedSeamSent,
}

impl Default for InterruptedWork {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        InterruptedWork {
            instance: None,
            fetched_for: None,
            generation: 0,
            last_op: 0,
            list: None,
            unavailable: None,
            selected: 0,
            scroll: 0,
            view: View::List,
            message: None,
            footer_hidden: false,
            pending: None,
            dismissal: None,
            list_op: None,
            show_op: None,
            question_seen: Vec::new(),
            recorder: None,
            tx,
            rx,
        }
    }
}

impl InterruptedWork {
    /// The listed items in the order shown: units, orchestrations and issue
    /// dispatches first, then plain agents ("Other agents", decision D1), each
    /// group most recent first as the daemon listed it.
    fn ordered(&self) -> Vec<&InterruptedItem> {
        let Some(list) = self.list.as_ref() else {
            return Vec::new();
        };
        let (agents, work): (Vec<&InterruptedItem>, Vec<&InterruptedItem>) = list
            .items
            .iter()
            .partition(|item| item.kind == ItemKind::Agent);
        work.into_iter().chain(agents).collect()
    }

    fn selected_item(&self) -> Option<&InterruptedItem> {
        self.ordered().get(self.selected).copied()
    }

    fn item(&self, id: &str) -> Option<&InterruptedItem> {
        self.list
            .as_ref()?
            .items
            .iter()
            .find(|item| item.id.as_str() == id)
    }

    /// A reclaim or dismissal is in flight.
    fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether the open view asks y/n now: reclaim's confirmation once its dry
    /// run says the worktree would be removed, and dismiss all's.
    fn asks(&self) -> bool {
        match &self.view {
            View::Reclaim { dry: Some(dry), .. } => !self.busy() && would_remove(dry),
            View::DismissAll { ids, .. } => !self.busy() && !ids.is_empty(),
            _ => false,
        }
    }

    /// A tag for a new request to the current daemon process.
    fn next_tag(&mut self) -> Tag {
        self.last_op += 1;
        Tag {
            generation: self.generation,
            op: self.last_op,
        }
    }

    /// Abandon a dry run in flight: its answer is no longer waited for.
    fn abandon_dry_run(&mut self) {
        if self.pending.is_some_and(|p| p.dry_run) {
            self.pending = None;
        }
    }
}

fn would_remove(outcome: &ReclaimOutcome) -> bool {
    outcome.verdict == ReclaimVerdict::WouldRemove && outcome.refusal.is_none()
}

/// `value` made safe to show on one line: control and bidi characters
/// escaped, cut to [`words::MAX_SHOWN_CHARS`] ([`words::display_value`], as
/// the CLI shows it).
fn clean(value: &str) -> String {
    words::display_value(value)
}

/// An item's name, or its branch, or its id.
fn item_name(item: &InterruptedItem) -> String {
    clean(
        item.name
            .as_deref()
            .or(item.branch.as_deref())
            .unwrap_or(item.id.as_str()),
    )
}

fn directory_label(item: &InterruptedItem) -> &'static str {
    if item.kind == ItemKind::Agent {
        words::LABEL_FOLDER
    } else {
        words::LABEL_WORKTREE
    }
}

fn last_entry(item: &InterruptedItem) -> String {
    format!(
        "{}: {}, {}",
        words::LABEL_LAST_ENTRY,
        clean(&item.last_entry.kind),
        words::format_time(item.last_entry.ts_ms)
    )
}

fn progress(item: &InterruptedItem) -> String {
    format!(
        "{}: {}",
        words::LABEL_PROGRESS,
        clean(&words::progress_line(&item.progress))
    )
}

fn failure(e: ClientError) -> String {
    match e {
        // The daemon's own sentence (an id that matches nothing, say).
        ClientError::Server(message) => clean(&message),
        other => format!("{} {}", words::DAEMON_FAILED, clean(&other.to_string())),
    }
}

/// `request`'s answer within `timeout`: `Ok(None)` when the daemon does not
/// support interrupted work.
async fn bounded<T>(
    timeout: Duration,
    request: impl Future<Output = Result<GatedQuery<T>, ClientError>>,
) -> Result<Option<T>, String> {
    match tokio::time::timeout(timeout, request).await {
        Err(_) => Err(format!(
            "{} {}",
            words::DAEMON_FAILED,
            ClientError::TimedOut(timeout)
        )),
        Ok(Err(e)) => Err(failure(e)),
        Ok(Ok(GatedQuery::Answered(answer))) => Ok(Some(answer)),
        Ok(Ok(GatedQuery::Unsupported)) => Ok(None),
    }
}

fn client() -> DaemonClient {
    DaemonClient::new(crate::endpoint_resolve::client_attach_socket_path())
}

/// Send `request` to the daemon process the list came from, on the tokio
/// runtime, and deliver its answer, tagged `tag`, to [`drain`]; the L1 seam
/// records it instead. `false`, with nothing sent, outside a runtime and the
/// seam.
fn send(iw: &mut InterruptedWork, tag: Tag, request: InterruptedSeamRequest) -> bool {
    let sent = InterruptedSeamSent {
        instance: iw.instance.clone(),
        request,
    };
    if let Some(log) = iw.recorder.as_mut() {
        log.push(Recorded { tag, sent });
        return true;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    let tx = iw.tx.clone();
    handle.spawn(async move {
        let _ = tx.send(ask(tag, sent).await);
    });
    true
}

/// The daemon's answer to `sent`, tagged `tag`.
async fn ask(tag: Tag, sent: InterruptedSeamSent) -> Answer {
    let InterruptedSeamSent { instance, request } = sent;
    let instance = instance.as_deref();
    match request {
        InterruptedSeamRequest::List { git } => Answer::Listed {
            tag,
            result: bounded(LIST_TIMEOUT, client().list_interrupted_work(git, instance)).await,
        },
        InterruptedSeamRequest::Show { id, reports_from } => {
            let result = bounded(
                SHOW_TIMEOUT,
                client().show_interrupted_page(&id, None, Some(reports_from), instance),
            )
            .await;
            Answer::Shown { tag, id, result }
        }
        InterruptedSeamRequest::Reclaim { id, dry_run } => {
            let result = bounded(
                RECLAIM_TIMEOUT,
                client().reclaim_interrupted(&id, dry_run, None, instance),
            )
            .await;
            Answer::Reclaimed {
                tag,
                id,
                dry_run,
                result,
            }
        }
        InterruptedSeamRequest::Dismiss { ids, all } => Answer::Dismissed {
            tag,
            result: bounded(
                DISMISS_TIMEOUT,
                client().dismiss_interrupted(ids, all, None, instance),
            )
            .await,
        },
    }
}

/// Ask for the list again; `git` asks for each item's Git and pull request
/// facts too.
fn request_list(iw: &mut InterruptedWork, git: bool) {
    let tag = iw.next_tag();
    iw.list_op = Some(tag.op);
    send(iw, tag, InterruptedSeamRequest::List { git });
}

/// PRD #1790 stage 2: fetch the list once for each daemon process the TUI's
/// event stream attaches to — after the TUI attaches, and after it reconnects
/// to another process. What an earlier process said is forgotten then, and
/// an answer to any request made of it is dropped when it arrives.
pub(super) fn sync(ui: &mut UiState, incarnation: &Incarnation) {
    let Some(process) = incarnation.as_deref() else {
        return;
    };
    let iw = &mut ui.interrupted;
    if iw.fetched_for.as_deref() == Some(process) {
        return;
    }
    let InterruptedWork {
        tx,
        rx,
        recorder,
        generation,
        last_op,
        pending,
        dismissal,
        ..
    } = std::mem::take(iw);
    // A dismissal in batches whose process went away: the rest are not sent
    // to the new one, and the person is told.
    let message = dismissal
        .filter(|batches| pending.is_some() && batches.not_sent() > 0)
        .map(|batches| {
            let stopped = words::DismissStopped {
                reason: words::DismissStopReason::Replaced,
                refusal: None,
                not_sent: batches.not_sent(),
            };
            match batches.done {
                Some(mut done) => {
                    done.stopped = Some(stopped);
                    dismiss_message(&done)
                }
                None => words::dismiss_stopped_sentence(&stopped),
            }
        });
    *iw = InterruptedWork {
        fetched_for: Some(process.to_string()),
        instance: Some(process.to_string()),
        generation: generation + 1,
        last_op,
        recorder,
        message,
        tx,
        rx,
        ..InterruptedWork::default()
    };
    request_list(iw, false);
}

/// PRD #1790 stage 2: apply the answers that arrived since the last frame.
pub(super) fn drain(ui: &mut UiState) {
    while let Ok(answer) = ui.interrupted.rx.try_recv() {
        apply(ui, answer);
    }
}

/// Apply one daemon answer: only one the TUI still waits for, from the
/// daemon process it is attached to; any other is dropped before it changes
/// anything.
pub(super) fn apply(ui: &mut UiState, answer: Answer) {
    let iw = &mut ui.interrupted;
    let tag = answer.tag();
    if tag.generation != iw.generation {
        return;
    }
    match answer {
        Answer::Listed { result, .. } => {
            if iw.list_op != Some(tag.op) {
                return;
            }
            iw.list_op = None;
            // The selection follows its item, not its row.
            let selected_id = iw.selected_item().map(|item| item.id.as_str().to_string());
            match result {
                Ok(Some(mut list)) => {
                    // A refresh without Git facts keeps the ones a list with
                    // them read, for the items still listed.
                    if let Some(old) = iw.list.as_ref() {
                        for item in list.items.iter_mut().filter(|i| i.git.is_none()) {
                            item.git = old
                                .items
                                .iter()
                                .find(|o| o.id == item.id)
                                .and_then(|o| o.git.clone());
                        }
                    }
                    iw.list = Some(list);
                    iw.unavailable = None;
                }
                Ok(None) => {
                    iw.list = None;
                    iw.unavailable = Some(words::UNSUPPORTED.to_string());
                }
                // A failed refresh keeps the list it had; a failed first
                // fetch shows nothing but in the overlay.
                Err(message) => {
                    if iw.list.is_none() {
                        iw.unavailable = Some(message);
                    }
                }
            }
            let ordered = iw.ordered();
            let still =
                selected_id.and_then(|id| ordered.iter().position(|item| item.id.as_str() == id));
            // Gone: the row that took its place, or the last.
            let count = ordered.len();
            iw.selected = still.unwrap_or_else(|| iw.selected.min(count.saturating_sub(1)));
            let gone = match &iw.view {
                View::Details { id, .. } | View::Reclaim { id, .. } => iw.item(id).is_none(),
                View::DismissAll { .. } | View::List => false,
            };
            if gone && !iw.busy() {
                iw.abandon_dry_run();
                iw.show_op = None;
                iw.view = View::List;
            }
        }
        Answer::Shown { id, result, .. } => {
            if iw.show_op != Some(tag.op) {
                return;
            }
            iw.show_op = None;
            let View::Details {
                id: open,
                show,
                asked,
                page,
                scroll,
                failed,
            } = &mut iw.view
            else {
                return;
            };
            if *open != id {
                return;
            }
            let why = match result {
                Ok(Some(shown)) => {
                    *show = Some(Box::new(shown));
                    *scroll = 0;
                    *failed = None;
                    return;
                }
                Ok(None) => words::UNSUPPORTED.to_string(),
                Err(message) => message,
            };
            // Failed: say why, and keep the page shown, if any, so the same
            // key asks again.
            *failed = Some(why);
            *page = show.as_ref().map(|show| show.reports_from);
            if show.is_none() {
                *asked = false;
            }
        }
        Answer::Reclaimed {
            id,
            dry_run,
            result,
            ..
        } => {
            if iw.pending.map(|p| p.op) != Some(tag.op) {
                return;
            }
            iw.pending = None;
            let open = matches!(&iw.view, View::Reclaim { id: open, .. } if *open == id);
            if dry_run {
                if !open {
                    return;
                }
                match result {
                    Ok(Some(outcome)) => {
                        if let View::Reclaim { dry, scroll, .. } = &mut iw.view {
                            *dry = Some(outcome);
                            *scroll = 0;
                        }
                        // A question that just appeared waits for its own frame.
                        ui.confirm_choices_drawn = None;
                        iw.question_seen.clear();
                    }
                    Ok(None) => {
                        iw.message = Some(words::UNSUPPORTED.to_string());
                        iw.view = View::List;
                    }
                    Err(message) => {
                        iw.message = Some(message);
                        iw.view = View::List;
                    }
                }
                return;
            }
            iw.message = Some(match result {
                Ok(Some(outcome)) => crate::interrupted_cli::reclaim_message(&outcome),
                Ok(None) => words::UNSUPPORTED.to_string(),
                Err(message) => message,
            });
            if open {
                iw.view = View::List;
            }
            request_list(iw, false);
        }
        Answer::Dismissed { result, .. } => {
            if iw.pending.map(|p| p.op) != Some(tag.op) {
                return;
            }
            iw.pending = None;
            let mut batches = iw.dismissal.take().unwrap_or_default();
            let message = match (result, batches.done.take()) {
                // A batch dismissed: add it up, and send the next if any.
                (Ok(Some(outcome)), done) if outcome.refusal.is_none() => {
                    let mut done = done.unwrap_or_default();
                    done.absorb(outcome);
                    if let Some(next) = batches.left.pop_front() {
                        batches.in_flight = next.len();
                        batches.done = Some(done);
                        iw.dismissal = Some(batches);
                        send_batch(iw, next);
                        return;
                    }
                    dismiss_message(&done)
                }
                // A later batch did not dismiss: the rest are not sent.
                (result, Some(mut done)) => {
                    let (reason, refusal) = match &result {
                        Ok(Some(outcome)) => (
                            words::DismissStopReason::Refused,
                            outcome.refusal.as_ref().map(|r| r.reason),
                        ),
                        Ok(None) => (words::DismissStopReason::Replaced, None),
                        Err(_) => (words::DismissStopReason::Failed, None),
                    };
                    done.stopped = Some(words::DismissStopped {
                        reason,
                        refusal,
                        not_sent: batches.not_sent(),
                    });
                    dismiss_message(&done)
                }
                // The first batch: as a single dismissal reads.
                (Ok(Some(outcome)), None) => dismiss_message(&outcome),
                (Ok(None), None) => words::UNSUPPORTED.to_string(),
                (Err(message), None) => message,
            };
            iw.message = Some(message);
            request_list(iw, false);
        }
    }
}

/// The sentence for a dismissal's outcome: a refusal of one of its ids, or
/// whether anything was dismissed, then why a dismissal sent in batches
/// stopped, when it did (verification finding B: a refusal does not hide it).
fn dismiss_message(outcome: &DismissOutcome) -> String {
    if let Some(refusal) = &outcome.refusal {
        return words::refusal_sentence(refusal.reason).to_string();
    }
    let said = if let Some(refused) = outcome.refused.first() {
        words::refusal_sentence(refused.refusal.reason)
    } else if outcome.dismissed.is_empty() && outcome.dismissed_omitted == 0 {
        words::NOTHING_DISMISSED
    } else {
        words::DISMISSED
    };
    match &outcome.stopped {
        Some(stopped) => format!("{said} {}", words::dismiss_stopped_sentence(stopped)),
        None => said.to_string(),
    }
}

/// PRD #1790 stage 2: open the overlay on the list, and ask for it again with
/// its Git facts. `current` is the event stream's daemon process, for a TUI
/// whose attach fetch has not answered.
pub(super) fn open(ui: &mut UiState, current: &Incarnation) {
    let iw = &mut ui.interrupted;
    if iw.instance.is_none() {
        iw.instance = current.as_deref().map(str::to_string);
    }
    iw.abandon_dry_run();
    iw.show_op = None;
    iw.view = View::List;
    iw.message = None;
    ui.mode = UiMode::InterruptedWork;
    request_list(iw, true);
}

/// PRD #1790 stage 2: hide the footer row until another daemon process.
pub(super) fn hide_footer(ui: &mut UiState) {
    ui.interrupted.footer_hidden = true;
}

/// The sentence the footer row shows, when it is shown: how many items the
/// daemon listed, or, for a list that came back empty but incomplete, that
/// some interrupted work may not be listed, so the row still leads to the
/// list (verification finding N4; the desktop draws the same banner).
pub(super) fn footer_sentence(ui: &UiState) -> Option<String> {
    let iw = &ui.interrupted;
    if iw.footer_hidden {
        return None;
    }
    if let Some(count) = listed_count(ui) {
        return Some(words::interrupted_footer(count));
    }
    iw.list
        .as_ref()
        .is_some_and(|list| list.incomplete)
        .then(|| words::INTERRUPTED_UNREAD.to_string())
}

/// Whether the daemon listed interrupted work or, its list being
/// incomplete, may hold some it could not read: what offers the footer row
/// and the restart notice's line.
pub(super) fn may_have_interrupted(ui: &UiState) -> bool {
    listed_count(ui).is_some()
        || ui
            .interrupted
            .list
            .as_ref()
            .is_some_and(|list| list.incomplete)
}

/// How many items the daemon listed, when it listed any.
pub(super) fn listed_count(ui: &UiState) -> Option<usize> {
    let list = ui.interrupted.list.as_ref()?;
    let count = list.items.len() + list.omitted;
    (count > 0).then_some(count)
}

/// Whether the open view asks y/n now.
pub(super) fn asks(ui: &UiState) -> bool {
    ui.mode == UiMode::InterruptedWork && ui.interrupted.asks()
}

/// Whether flushed frames have shown every row of the open view's y/n
/// question since it appeared, whether in one frame or over several as it
/// was scrolled through.
pub(super) fn question_shown(ui: &UiState) -> bool {
    let seen = &ui.interrupted.question_seen;
    !seen.is_empty() && seen.iter().all(|(_, seen)| *seen)
}

/// Read the y/n question's rows back from the frame as flushed: each row it
/// shows whole counts as seen from now on (`question_seen`).
pub(super) fn record_question(ui: &mut UiState, choices: &ConfirmChoices, flushed: &Buffer) {
    let seen = &mut ui.interrupted.question_seen;
    for ((x, row, text), (drawn, was_seen)) in choices.cells.iter().zip(seen.iter_mut()) {
        if drawn == text && row.is_some_and(|y| choices.intact(*x, y, text, flushed)) {
            *was_seen = true;
        }
    }
}

/// Keys while the overlay is up. Ctrl+C keeps its PRD #40 safety net.
pub(super) fn handle_key(key: KeyEvent, ui: &mut UiState, kb: &KeybindingConfig) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
    }
    // The footer row offers its hide key here too (`interrupted_keys_act`).
    if footer_sentence(ui).is_some() && kb.matches(KbAction::HideInterruptedWork, &key) {
        return Action::HideInterruptedWork;
    }
    let shifted = |c: char| {
        key.code == KeyCode::Char(c.to_ascii_uppercase())
            || (key.code == KeyCode::Char(c) && key.modifiers.contains(KeyModifiers::SHIFT))
    };
    let press = match &ui.interrupted.view {
        View::List => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Some(Press::Close),
            _ if kb.matches(KbAction::InterruptedWork, &key) => Some(Press::Close),
            KeyCode::Down | KeyCode::Char('j') => Some(Press::Down),
            KeyCode::Up | KeyCode::Char('k') => Some(Press::Up),
            KeyCode::Enter => Some(Press::Open),
            _ if shifted('d') => Some(Press::DismissAll),
            KeyCode::Char('r') => Some(Press::Reclaim),
            KeyCode::Char('d') => Some(Press::Dismiss),
            _ => None,
        },
        View::Details { .. } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Some(Press::Back),
            KeyCode::Enter => Some(Press::Open),
            KeyCode::Char('n') => Some(Press::NextReports),
            KeyCode::Char('p') => Some(Press::PreviousReports),
            KeyCode::Down | KeyCode::Char('j') => Some(Press::Down),
            KeyCode::Up | KeyCode::Char('k') => Some(Press::Up),
            KeyCode::PageDown => Some(Press::PageDown),
            KeyCode::PageUp => Some(Press::PageUp),
            _ => None,
        },
        View::Reclaim { .. } | View::DismissAll { .. } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Char('N') => {
                Some(Press::Back)
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Press::Yes),
            // A confirmation longer than its popup scrolls to its question.
            KeyCode::Down | KeyCode::Char('j') => Some(Press::Down),
            KeyCode::Up | KeyCode::Char('k') => Some(Press::Up),
            KeyCode::PageDown => Some(Press::PageDown),
            KeyCode::PageUp => Some(Press::PageUp),
            _ => None,
        },
    };
    press.map_or(Action::Continue, Action::InterruptedWork)
}

/// How far PageUp/PageDown scroll the details and the confirmations.
const DETAILS_PAGE: usize = 10;

/// `scroll` moved as `press` asks.
fn scrolled(scroll: usize, press: Press) -> usize {
    match press {
        Press::Down => scroll.saturating_add(1),
        Press::Up => scroll.saturating_sub(1),
        Press::PageDown => scroll.saturating_add(DETAILS_PAGE),
        Press::PageUp => scroll.saturating_sub(DETAILS_PAGE),
        _ => scroll,
    }
}

/// Do what `press` asks.
pub(super) fn press(ui: &mut UiState, press: Press) {
    if press == Press::Close {
        let iw = &mut ui.interrupted;
        iw.abandon_dry_run();
        iw.show_op = None;
        iw.view = View::List;
        if ui.mode == UiMode::InterruptedWork {
            ui.mode = UiMode::Normal;
        }
        return;
    }
    let iw = &mut ui.interrupted;
    let scrolls = matches!(
        press,
        Press::Down | Press::Up | Press::PageDown | Press::PageUp
    );
    match iw.view.clone() {
        View::List => match press {
            Press::Down => {
                let count = iw.ordered().len();
                if count > 0 {
                    iw.selected = (iw.selected + 1).min(count - 1);
                }
            }
            Press::Up => iw.selected = iw.selected.saturating_sub(1),
            Press::Open => {
                if let Some(item) = iw.selected_item() {
                    iw.view = View::Details {
                        id: item.id.as_str().to_string(),
                        show: None,
                        asked: false,
                        scroll: 0,
                        page: None,
                        failed: None,
                    };
                }
            }
            Press::Reclaim => {
                let Some(item) = iw.selected_item() else {
                    return;
                };
                // Decision D1: a plain agent has no worktree the deck created.
                if iw.busy() || item.kind == ItemKind::Agent {
                    return;
                }
                let id = item.id.as_str().to_string();
                iw.view = View::Reclaim {
                    id: id.clone(),
                    dry: None,
                    scroll: 0,
                };
                reclaim(iw, id, true);
            }
            Press::Dismiss => {
                if iw.busy() {
                    return;
                }
                if let Some(item) = iw.selected_item() {
                    let ids = vec![item.id.as_str().to_string()];
                    dismiss(iw, ids);
                }
            }
            Press::DismissAll => {
                let ids: Vec<String> = iw
                    .ordered()
                    .iter()
                    .map(|item| item.id.as_str().to_string())
                    .collect();
                if !iw.busy() && !ids.is_empty() {
                    iw.view = View::DismissAll { ids, scroll: 0 };
                    // A question that just appeared waits for its own frame.
                    ui.confirm_choices_drawn = None;
                    ui.interrupted.question_seen.clear();
                }
            }
            _ => {}
        },
        View::Details {
            id, asked, show, ..
        } => match press {
            _ if scrolls => {
                if let View::Details { scroll, .. } = &mut iw.view {
                    *scroll = scrolled(*scroll, press);
                }
            }
            Press::Open => {
                let has_reports = iw.item(&id).is_some_and(|item| !item.reports.is_empty());
                if asked || !has_reports {
                    return;
                }
                request_page(iw, id, 0);
            }
            // Reviewer 1: every kept report can be reached, a page at a time.
            Press::NextReports => {
                if let Some(next) = show.and_then(|show| show.next_reports_from) {
                    request_page(iw, id, next);
                }
            }
            Press::PreviousReports => {
                if let Some(from) = show.map(|show| show.reports_from).filter(|from| *from > 0) {
                    request_page(iw, id, from.saturating_sub(words::MAX_SHOWN_REPORTS));
                }
            }
            Press::Back => {
                iw.show_op = None;
                iw.view = View::List;
            }
            _ => {}
        },
        View::Reclaim { id, dry, .. } => match press {
            _ if scrolls => {
                if let View::Reclaim { scroll, .. } = &mut iw.view {
                    *scroll = scrolled(*scroll, press);
                }
            }
            Press::Yes => {
                if !iw.busy() && dry.as_ref().is_some_and(would_remove) {
                    reclaim(iw, id, false);
                }
            }
            // A reclaim in flight still reports its sentence in the list; a
            // dry run in flight is abandoned.
            Press::Back => {
                iw.abandon_dry_run();
                iw.view = View::List;
            }
            _ => {}
        },
        View::DismissAll { ids, .. } => match press {
            _ if scrolls => {
                if let View::DismissAll { scroll, .. } = &mut iw.view {
                    *scroll = scrolled(*scroll, press);
                }
            }
            Press::Yes => {
                if !iw.busy() {
                    dismiss(iw, ids);
                }
            }
            Press::Back => iw.view = View::List,
            _ => {}
        },
    }
}

/// Ask for the page of the item `id`'s kept reports, and their text, that
/// starts at its `from`-th report; only the newest such request's answer is
/// shown.
fn request_page(iw: &mut InterruptedWork, id: String, from: usize) {
    let tag = iw.next_tag();
    if let View::Details { asked, page, .. } = &mut iw.view {
        *asked = true;
        *page = Some(from);
    }
    iw.show_op = Some(tag.op);
    send(
        iw,
        tag,
        InterruptedSeamRequest::Show {
            id,
            reports_from: from,
        },
    );
}

/// Reclaim the item `id`, or with `dry_run` ask what that would do.
fn reclaim(iw: &mut InterruptedWork, id: String, dry_run: bool) {
    let tag = iw.next_tag();
    iw.pending = Some(Pending {
        op: tag.op,
        dry_run,
    });
    if !send(iw, tag, InterruptedSeamRequest::Reclaim { id, dry_run }) {
        iw.pending = None;
    }
}

/// Dismiss exactly the items `ids` name: never "all", so an item listed
/// after the person was asked is not dismissed.
///
/// The ids go in the batches the daemon admits ([`words::dismiss_batches`]),
/// one at a time: [`apply`] sends the next once the one before answered.
fn dismiss(iw: &mut InterruptedWork, ids: Vec<String>) {
    let mut left: std::collections::VecDeque<Vec<String>> = words::dismiss_batches(&ids).into();
    let first = left.pop_front().unwrap_or_default();
    iw.dismissal = Some(Batches {
        in_flight: first.len(),
        left,
        done: None,
    });
    iw.view = View::List;
    send_batch(iw, first);
}

/// Send one batch of a dismissal; it is the request [`InterruptedWork::pending`]
/// waits for.
fn send_batch(iw: &mut InterruptedWork, ids: Vec<String>) {
    let tag = iw.next_tag();
    iw.pending = Some(Pending {
        op: tag.op,
        dry_run: false,
    });
    if !send(iw, tag, InterruptedSeamRequest::Dismiss { ids, all: false }) {
        iw.pending = None;
        iw.dismissal = None;
    }
}

// ---------------------------------------------------------------------------
// Footer
// ---------------------------------------------------------------------------

/// The footer row's `[Show]` and `[Hide]` buttons, naming their keys only
/// when `keys` (where the keys act).
fn footer_buttons(keybindings: &KeybindingConfig, keys: bool) -> [Button; 2] {
    let shortcut = |action| {
        if keys {
            button_shortcut_label(keybindings, action)
        } else {
            String::new()
        }
    };
    [
        Button::new(
            "Show",
            shortcut(KbAction::InterruptedWork),
            Action::OpenInterruptedWork,
            true,
        ),
        Button::new(
            "Hide",
            shortcut(KbAction::HideInterruptedWork),
            Action::HideInterruptedWork,
            true,
        ),
    ]
}

fn footer_buttons_width(keybindings: &KeybindingConfig, keys: bool) -> u16 {
    let buttons = footer_buttons(keybindings, keys);
    buttons
        .iter()
        .map(|b| b.display_label().chars().count() as u16)
        .sum::<u16>()
        + buttons.len() as u16
}

/// How many footer rows the interrupted work takes at `width`: none, one, or
/// two when its sentence and buttons do not fit side by side.
pub(super) fn footer_rows(ui: &UiState, keys: bool, width: u16) -> u16 {
    let Some(sentence) = footer_sentence(ui) else {
        return 0;
    };
    let sentence = unicode_width::UnicodeWidthStr::width(sentence.as_str());
    let buttons = usize::from(footer_buttons_width(&ui.keybindings, keys));
    if sentence + buttons <= usize::from(width) {
        1
    } else {
        2
    }
}

/// Draw the footer row(s) in `area`, answering the buttons' click targets.
pub(super) fn render_footer(
    frame: &mut Frame,
    ui: &UiState,
    keys: bool,
    area: Rect,
) -> Vec<(Action, Rect)> {
    let Some(sentence) = footer_sentence(ui) else {
        return Vec::new();
    };
    frame.render_widget(Clear, area);
    let buttons = footer_buttons(&ui.keybindings, keys);
    let text_area = if area.height >= 2 {
        Rect { height: 1, ..area }
    } else {
        Rect {
            width: area
                .width
                .saturating_sub(footer_buttons_width(&ui.keybindings, keys)),
            ..area
        }
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            sentence,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        text_area,
    );
    let button_row = Rect {
        y: area.y + area.height.saturating_sub(1),
        height: 1,
        ..area
    };
    render_right_aligned_buttons(frame, &buttons, button_row)
}

/// The restart notice list's line leading to the interrupted work list, when
/// there is any, or when an incomplete list may hold some (PR #2431 review).
pub(super) fn restart_notice_hint(ui: &UiState) -> Option<String> {
    let key = display_notation(&ui.keybindings, KbAction::InterruptedWork);
    if let Some(count) = listed_count(ui) {
        return Some(format!(
            "{} · {key} to show them",
            words::interrupted_footer(count)
        ));
    }
    may_have_interrupted(ui)
        .then(|| format!("{} · {key} to show the list", words::INTERRUPTED_UNREAD))
}

// ---------------------------------------------------------------------------
// Overlay
// ---------------------------------------------------------------------------

fn heading() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn notice() -> Style {
    Style::default().fg(Color::Yellow)
}

/// `text` wrapped to `width`, each row indented by `indent` and styled.
fn wrapped(text: &str, width: usize, indent: &str, style: Style) -> Vec<Line<'static>> {
    crate::upgrade_dialog::wrap(text, width.saturating_sub(indent.len()).max(1))
        .into_iter()
        .map(|row| Line::styled(format!("{indent}{row}"), style))
        .collect()
}

/// One row, cut to `width` with an ellipsis.
fn row(text: String, width: usize, style: Style) -> Line<'static> {
    Line::styled(truncate_with_ellipsis(&text, width), style)
}

/// One list item's rows: its name, kind and branch; its directory; who
/// started it and its last entry; and, selected, its progress.
fn item_rows(item: &InterruptedItem, selected: bool, width: usize) -> Vec<Line<'static>> {
    let mut header = format!(
        "{} {} · {}",
        if selected { "▶" } else { " " },
        item_name(item),
        words::kind_label(item.kind)
    );
    if item.name.is_some()
        && let Some(branch) = &item.branch
    {
        header.push_str(&format!(" · {}", clean(branch)));
    }
    let header_style = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        text_primary()
    };
    let mut rows = vec![row(format!("  {header}"), width, header_style)];
    if let Some(worktree) = &item.worktree {
        let label = directory_label(item);
        let room = width.saturating_sub(6 + label.len() + 2);
        rows.push(row(
            format!(
                "      {label}: {}",
                truncate_path_head(&clean(worktree), room)
            ),
            width,
            text_primary(),
        ));
    }
    let mut facts = Vec::new();
    if let Some(launcher) = &item.launcher {
        facts.push(format!("{}: {}", words::LABEL_STARTED_BY, clean(launcher)));
    }
    facts.push(last_entry(item));
    rows.push(row(
        format!("      {}", facts.join(" · ")),
        width,
        text_dim(),
    ));
    if selected {
        rows.push(row(
            format!("      {}", progress(item)),
            width,
            text_primary(),
        ));
    }
    rows
}

/// The list view's body, with the first and last row of the selected item.
fn list_body(iw: &InterruptedWork, width: usize) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let Some(list) = iw.list.as_ref() else {
        if let Some(why) = &iw.unavailable {
            lines.extend(wrapped(why, width, "  ", notice()));
        }
        return (lines, None);
    };
    if list.incomplete {
        lines.extend(wrapped(words::LIST_INCOMPLETE, width, "  ", notice()));
        lines.push(Line::from(""));
    }
    let ordered = iw.ordered();
    if ordered.is_empty() {
        lines.push(Line::styled(
            format!("  {}", words::NOTHING_INTERRUPTED),
            text_primary(),
        ));
        return (lines, None);
    }
    let mut selected_rows = None;
    let mut grouped = false;
    for (index, item) in ordered.iter().enumerate() {
        if item.kind == ItemKind::Agent && !grouped {
            grouped = true;
            if index > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::styled(
                format!("  {}", words::OTHER_AGENTS_GROUP),
                heading(),
            ));
        }
        let selected = index == iw.selected;
        let first = lines.len();
        lines.extend(item_rows(item, selected, width));
        if selected {
            selected_rows = Some((first, lines.len() - 1));
        }
    }
    if list.omitted > 0 {
        lines.push(Line::from(""));
        lines.push(row(
            format!("  {}", words::omitted_sentence(list.omitted)),
            width,
            text_dim(),
        ));
    }
    (lines, selected_rows)
}

/// The details view's body.
fn details_body(
    item: &InterruptedItem,
    git: Option<&GitFacts>,
    show: Option<&InterruptedShow>,
    asked: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![row(
        format!("  {} · {}", item_name(item), words::kind_label(item.kind)),
        width,
        heading(),
    )];
    let mut fact = |label: &str, value: String| {
        lines.extend(wrapped(
            &format!("{label}: {value}"),
            width,
            "    ",
            text_primary(),
        ));
    };
    if let Some(issue) = item.issue {
        let repo = item.repo.as_deref().map(clean).unwrap_or_default();
        fact(words::LABEL_ISSUE, format!("{repo}#{issue}"));
    }
    if let Some(branch) = &item.branch {
        fact(words::LABEL_BRANCH, clean(branch));
    }
    if let Some(worktree) = &item.worktree {
        fact(directory_label(item), clean(worktree));
    }
    if let Some(launcher) = &item.launcher {
        fact(words::LABEL_STARTED_BY, clean(launcher));
    }
    fact(
        words::LABEL_LAST_ENTRY,
        format!(
            "{}, {}",
            clean(&item.last_entry.kind),
            words::format_time(item.last_entry.ts_ms)
        ),
    );
    fact(
        words::LABEL_PROGRESS,
        clean(&words::progress_line(&item.progress)),
    );
    if let Some(git) = git {
        fact(words::LABEL_GIT, words::git_facts_line(git));
    }
    for report in &item.reports {
        fact(
            words::LABEL_REPORT,
            format!(
                "{}, {}",
                clean(&report.report_id),
                words::kept_until(report.kept_until_ms)
            ),
        );
    }
    match show.filter(|_| asked) {
        // A page of the item's kept reports (reviewer 1): which ones it
        // holds, and how many come after it.
        Some(show) => {
            let total = item.reports.len() + item.reports_omitted;
            if (show.reports_from > 0 || show.next_reports_from.is_some())
                && !item.reports.is_empty()
            {
                lines.extend(wrapped(
                    &words::report_range_sentence(
                        show.reports_from + 1,
                        show.reports_from + item.reports.len(),
                        total,
                    ),
                    width,
                    "    ",
                    text_primary(),
                ));
            }
            let after = item.reports_omitted.saturating_sub(show.reports_from);
            if after > 0 {
                lines.extend(wrapped(
                    &words::more_reports_sentence(after, item.id.as_str()),
                    width,
                    "    ",
                    text_dim(),
                ));
            }
        }
        None if item.reports_omitted > 0 => {
            lines.extend(wrapped(
                &words::more_reports_sentence(item.reports_omitted, item.id.as_str()),
                width,
                "    ",
                text_dim(),
            ));
        }
        None => {}
    }
    if item.cut {
        lines.extend(wrapped(words::ITEM_CUT, width, "    ", text_dim()));
    }
    if !asked {
        return lines;
    }
    let Some(show) = show else {
        return lines;
    };
    if show.bodies_omitted > 0 {
        lines.push(Line::from(""));
        lines.extend(wrapped(
            &words::bodies_omitted_sentence(show.bodies_omitted, item.id.as_str()),
            width,
            "  ",
            text_dim(),
        ));
    }
    for report in &show.reports {
        lines.push(Line::from(""));
        lines.push(row(
            format!("  {} {}", words::LABEL_REPORT, clean(&report.report_id)),
            width,
            heading(),
        ));
        match &report.body {
            Some(body) => {
                if report.cut {
                    lines.extend(wrapped(words::REPORT_CUT, width, "  ", notice()));
                }
                // The daemon framed it as untrusted text written by an agent;
                // it is shown as framed, each of its lines made safe: tabs as
                // spaces, control and bidi characters escaped, as
                // `words::display_value` shows a value.
                for line in body.lines() {
                    let line =
                        crate::untrusted_text::escape_control_and_bidi(&line.replace('\t', " "));
                    lines.extend(wrapped(&line, width, "  ", text_dim()));
                }
            }
            None => lines.extend(wrapped(
                &format!(
                    "{} {}",
                    words::REPORT_UNREADABLE,
                    clean(report.error.as_deref().unwrap_or_default())
                ),
                width,
                "  ",
                notice(),
            )),
        }
    }
    lines
}

/// A y/n question's rows: each row's index in its body, and its text after
/// the indent.
type Question = Vec<(usize, String)>;

/// Reclaim's confirmation body, and the row of its y/n question when it asks.
fn reclaim_body(
    iw: &InterruptedWork,
    id: &str,
    dry: Option<&ReclaimOutcome>,
    width: usize,
) -> (Vec<Line<'static>>, Question) {
    let mut lines = Vec::new();
    if let Some(item) = iw.item(id) {
        lines.push(row(
            format!(
                "  {} {} · {}",
                words::RECLAIM_ACTION,
                item_name(item),
                words::kind_label(item.kind)
            ),
            width,
            heading(),
        ));
    }
    lines.extend(wrapped(
        &format!("{}: {}", words::RECLAIM_ACTION, words::RECLAIM_EXPLANATION),
        width,
        "  ",
        text_primary(),
    ));
    let Some(dry) = dry else {
        return (lines, Vec::new());
    };
    lines.push(Line::from(""));
    lines.extend(wrapped(
        &clean(&crate::interrupted_cli::reclaim_message(dry)),
        width,
        "  ",
        notice(),
    ));
    if let Some(worktree) = &dry.worktree {
        lines.extend(wrapped(
            &format!("{}: {}", words::LABEL_WORKTREE, clean(worktree)),
            width,
            "    ",
            text_primary(),
        ));
    }
    if let Some(branch) = &dry.branch {
        lines.extend(wrapped(
            &format!("{}: {}", words::LABEL_BRANCH, clean(branch)),
            width,
            "    ",
            text_primary(),
        ));
    }
    for nested in &dry.nested_worktrees {
        lines.extend(wrapped(
            &format!("{}: {}", words::LABEL_NESTED_WORKTREE, clean(nested)),
            width,
            "    ",
            text_primary(),
        ));
    }
    if let Some(sentence) = dry.unpushed_commits.and_then(words::unpushed_sentence) {
        lines.extend(wrapped(&sentence, width, "  ", text_primary()));
    }
    if !iw.asks() {
        return (lines, Vec::new());
    }
    lines.push(Line::from(""));
    let question = format!("{}? y/n", words::RECLAIM_ACTION);
    let at = lines.len();
    lines.push(Line::styled(format!("  {question}"), heading()));
    (lines, vec![(at, question)])
}

/// Dismiss all's confirmation body: its question, wrapped, every row of it
/// a row of the y/n question.
fn dismiss_all_body(
    iw: &InterruptedWork,
    count: usize,
    width: usize,
) -> (Vec<Line<'static>>, Question) {
    let text = words::dismiss_all_question(count).trim_end().to_string();
    let rows = crate::upgrade_dialog::wrap(&text, width.saturating_sub(2).max(1));
    let lines = rows
        .iter()
        .map(|row| Line::styled(format!("  {row}"), heading()))
        .collect();
    let question = if iw.asks() {
        rows.into_iter().enumerate().collect()
    } else {
        Vec::new()
    };
    (lines, question)
}

/// The hints under the body; `overflow`: the body is longer than its room.
fn hints(iw: &InterruptedWork, overflow: bool) -> String {
    let lower = |word: &str| word.to_lowercase();
    let scroll = if overflow { " · j/k scroll" } else { "" };
    match &iw.view {
        View::List => {
            let mut parts = Vec::new();
            if let Some(item) = iw.selected_item() {
                parts.push("j/k select".to_string());
                parts.push("Enter details".to_string());
                if item.kind != ItemKind::Agent {
                    parts.push(format!("r {}", lower(words::RECLAIM_ACTION)));
                }
                parts.push(format!("d {}", lower(words::DISMISS_ACTION)));
                parts.push(format!("D {}", lower(words::DISMISS_ALL_ACTION)));
            }
            parts.push("Esc close".to_string());
            format!("  {}", parts.join(" · "))
        }
        View::Details {
            id, asked, show, ..
        } => {
            let mut parts = Vec::new();
            if !asked && iw.item(id).is_some_and(|item| !item.reports.is_empty()) {
                parts.push("Enter show reports".to_string());
            }
            if let Some(show) = show.as_deref().filter(|_| *asked) {
                if show.next_reports_from.is_some() {
                    parts.push("n next reports".to_string());
                }
                if show.reports_from > 0 {
                    parts.push("p previous reports".to_string());
                }
            }
            parts.push("j/k scroll".to_string());
            parts.push("Esc back".to_string());
            format!("  {}", parts.join(" · "))
        }
        View::Reclaim { .. } if iw.asks() => {
            format!("  y {} · n back{scroll}", lower(words::RECLAIM_ACTION))
        }
        View::DismissAll { .. } if iw.asks() => {
            format!("  y {} · n back{scroll}", lower(words::DISMISS_ALL_ACTION))
        }
        View::Reclaim { .. } | View::DismissAll { .. } => format!("  Esc back{scroll}"),
    }
}

/// Draw the overlay in `area`, answering its click targets and, for a
/// confirmation that asks, where it put its y/n question.
pub(super) fn render_overlay(
    frame: &mut Frame,
    area: Rect,
    ui: &mut UiState,
) -> (Vec<(Action, Rect)>, Option<ConfirmDrawing>) {
    let iw = &mut ui.interrupted;
    let popup_width = POPUP_MAX_WIDTH
        .min(area.width.saturating_sub(2))
        .max(area.width.min(40));
    // Inside the border, with one column of margin on the right.
    let width = usize::from(popup_width.saturating_sub(3));
    let (body, selected_rows, question) = match &iw.view {
        View::List => {
            let (body, selected) = list_body(iw, width);
            (body, selected, Vec::new())
        }
        View::Details {
            id, show, asked, ..
        } => {
            let item = show.as_ref().map(|s| &s.item).or_else(|| iw.item(id));
            // A show answers its item without Git facts: the list's are
            // kept (PR #2431 review), whichever of the two arrived last.
            let git = item
                .and_then(|item| item.git.as_ref())
                .or_else(|| iw.item(id).and_then(|listed| listed.git.as_ref()));
            let body = item
                .map(|item| details_body(item, git, show.as_deref(), *asked, width))
                .unwrap_or_default();
            (body, None, Vec::new())
        }
        View::Reclaim { id, dry, .. } => {
            let (body, question) = reclaim_body(iw, id, dry.as_ref(), width);
            (body, None, question)
        }
        View::DismissAll { ids, .. } => {
            let (body, question) = dismiss_all_body(iw, ids.len(), width);
            (body, None, question)
        }
    };
    // Every message is drawn escaped, whichever path it took here.
    let message: Vec<Line<'static>> = match (&iw.view, &iw.message) {
        (View::List, Some(message)) => wrapped(&clean(message), width, "  ", notice()),
        (
            View::Details {
                failed: Some(why), ..
            },
            _,
        ) => wrapped(&clean(why), width, "  ", notice()),
        _ => Vec::new(),
    };
    // A blank row, then the body, then a blank row, the message and the hint:
    // plus the border.
    let footer_height = message.len() + 2;
    let wanted = body.len() + 1 + footer_height + 2;
    let popup_height = wanted.min(usize::from(area.height.saturating_sub(2))) as u16;
    let x = area.x + area.width.saturating_sub(popup_width) / 2;
    let y = area.y + area.height.saturating_sub(popup_height) / 2;
    let popup = Rect::new(x, y, popup_width, popup_height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", words::INTERRUPTED_WORK_TITLE))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let body_height =
        usize::from(inner.height).saturating_sub(footer_height.min(usize::from(inner.height)) + 1);
    let last_screenful = body.len().saturating_sub(body_height);
    // Keep the selected item in view; the details and the confirmations
    // scroll where the keys put them, up to their last screenful.
    let scroll = match &mut iw.view {
        View::List => {
            if let Some((first, last)) = selected_rows {
                if first < iw.scroll {
                    iw.scroll = first;
                } else if last >= iw.scroll + body_height {
                    iw.scroll = last + 1 - body_height.min(last + 1);
                }
            }
            iw.scroll = iw.scroll.min(last_screenful);
            iw.scroll
        }
        View::Details { scroll, .. }
        | View::Reclaim { scroll, .. }
        | View::DismissAll { scroll, .. } => {
            *scroll = (*scroll).min(last_screenful);
            *scroll
        }
    };
    let hint = hints(iw, body.len() > body_height);
    let shown: Vec<Line<'static>> = body.into_iter().skip(scroll).take(body_height).collect();
    let mut lines = vec![Line::from("")];
    lines.extend(shown);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            (body_height as u16 + 1).min(inner.height),
        ),
    );
    let mut footer = vec![Line::from("")];
    footer.extend(message);
    footer.push(row(hint, width + 1, text_primary()));
    let footer_rows = (footer.len() as u16).min(inner.height);
    frame.render_widget(
        Paragraph::new(footer),
        Rect::new(
            inner.x,
            inner.y + inner.height - footer_rows,
            inner.width,
            footer_rows,
        ),
    );

    // The y/n question: the confirmation's one choice, counted only once a
    // flushed frame shows every row of it (`refuse_undrawn_choice`); a row
    // scrolled out of the body is not shown.
    let same_rows = iw.question_seen.len() == question.len()
        && iw
            .question_seen
            .iter()
            .zip(&question)
            .all(|((seen, _), (_, text))| seen == text);
    if !same_rows {
        iw.question_seen = question
            .iter()
            .map(|(_, text)| (text.clone(), false))
            .collect();
    }
    let confirmation = (!question.is_empty()).then(|| {
        let cells = question
            .into_iter()
            .map(|(at, text)| {
                let row = at
                    .checked_sub(scroll)
                    .filter(|shown_at| *shown_at < body_height)
                    .and_then(|shown_at| u16::try_from(shown_at + 1).ok())
                    .and_then(|r| inner.y.checked_add(r));
                (inner.x + 2, row, text)
            })
            .collect();
        ConfirmDrawing {
            buttons: Vec::new(),
            choices: ConfirmChoices { inner, cells },
        }
    });
    let close_row = Rect::new(
        inner.x,
        inner.y + inner.height - footer_rows,
        inner.width,
        1,
    );
    let buttons = if confirmation.is_none() && footer_rows > 0 {
        let close = [Button::new(
            "Close",
            "",
            Action::InterruptedWork(Press::Close),
            true,
        )];
        render_right_aligned_buttons(frame, &close, close_row)
    } else {
        Vec::new()
    };
    (buttons, confirmation)
}

// ---------------------------------------------------------------------------
// L1 seam
// ---------------------------------------------------------------------------

/// The L1 seam: record every request instead of sending it.
pub(super) fn seam_record(ui: &mut UiState) {
    ui.interrupted.recorder = Some(Vec::new());
}

/// The requests the L1 seam recorded, in order.
pub(super) fn seam_sent(ui: &UiState) -> Vec<InterruptedSeamSent> {
    ui.interrupted
        .recorder
        .iter()
        .flatten()
        .map(|r| r.sent.clone())
        .collect()
}

/// The answer the L1 seam hands in for `step`, tagged as the daemon would
/// tag it: to the request at index `to` of the record, or else to the newest
/// recorded request of the step's kind. `None` when there is no such
/// request, or `step` is no answer.
pub(super) fn seam_answer(
    ui: &UiState,
    to: Option<usize>,
    step: &InterruptedSeamStep,
) -> Option<Answer> {
    let log = ui.interrupted.recorder.as_ref()?;
    let wanted = |r: &&Recorded| match step {
        InterruptedSeamStep::Reclaimed(outcome) => matches!(
            &r.sent.request,
            InterruptedSeamRequest::Reclaim { dry_run, .. } if *dry_run == outcome.dry_run
        ),
        _ => seam_kind(step).is_some_and(|kind| r.sent.request.kind() == kind),
    };
    let recorded = match to {
        Some(index) => log.get(index)?,
        None => log.iter().rev().find(wanted)?,
    };
    let tag = recorded.tag;
    let (id, dry_run) = match &recorded.sent.request {
        InterruptedSeamRequest::Show { id, .. } => (id.clone(), false),
        InterruptedSeamRequest::Reclaim { id, dry_run } => (id.clone(), *dry_run),
        _ => (String::new(), false),
    };
    // A failed (`Err`) or unsupported (`Ok`) answer of the request's kind.
    let failed = |result: Result<(), String>| -> Answer {
        fn none<T>(result: &Result<(), String>) -> Result<Option<T>, String> {
            result.clone().map(|()| None)
        }
        match recorded.sent.request.kind() {
            InterruptedSeamKind::List => Answer::Listed {
                tag,
                result: none(&result),
            },
            InterruptedSeamKind::Show => Answer::Shown {
                tag,
                id: id.clone(),
                result: none(&result),
            },
            InterruptedSeamKind::Reclaim => Answer::Reclaimed {
                tag,
                id: id.clone(),
                dry_run,
                result: none(&result),
            },
            InterruptedSeamKind::Dismiss => Answer::Dismissed {
                tag,
                result: none(&result),
            },
        }
    };
    Some(match step {
        InterruptedSeamStep::Listed(list) => Answer::Listed {
            tag,
            result: Ok(Some(list.clone())),
        },
        InterruptedSeamStep::Shown(show) => Answer::Shown {
            tag,
            id,
            result: Ok(Some(show.clone())),
        },
        InterruptedSeamStep::Reclaimed(outcome) => Answer::Reclaimed {
            tag,
            id,
            dry_run: outcome.dry_run,
            result: Ok(Some(outcome.clone())),
        },
        InterruptedSeamStep::Dismissed(outcome) => Answer::Dismissed {
            tag,
            result: Ok(Some(outcome.clone())),
        },
        InterruptedSeamStep::Failed(_, message) => failed(Err(message.clone())),
        InterruptedSeamStep::Unsupported(_) => failed(Ok(())),
        _ => return None,
    })
}

/// The kind of request `step` answers.
fn seam_kind(step: &InterruptedSeamStep) -> Option<InterruptedSeamKind> {
    Some(match step {
        InterruptedSeamStep::Listed(_) => InterruptedSeamKind::List,
        InterruptedSeamStep::Shown(_) => InterruptedSeamKind::Show,
        InterruptedSeamStep::Reclaimed(_) => InterruptedSeamKind::Reclaim,
        InterruptedSeamStep::Dismissed(_) => InterruptedSeamKind::Dismiss,
        InterruptedSeamStep::Failed(kind, _) | InterruptedSeamStep::Unsupported(kind) => *kind,
        _ => return None,
    })
}
