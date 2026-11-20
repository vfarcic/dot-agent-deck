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
//!   `[Hide]` buttons.
//! - **Overlay** (`UiMode::InterruptedWork`): the list, an item's details, and
//!   the reclaim and dismiss-all confirmations. In the details, `Enter` shows
//!   the first page of the item's kept reports with their text, and `n`/`p`
//!   the next and previous page, so a report past the ones the list carries
//!   can be reached; only the page last asked for is shown.
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
    self as words, DismissOutcome, InterruptedItem, InterruptedList, InterruptedShow, ItemKind,
    ReclaimOutcome, ReclaimVerdict,
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
    /// One item's details. `asked`: its kept reports' text was asked for.
    /// `page`: where the page of kept reports last asked for starts; only
    /// an answer for that page is shown.
    Details {
        id: String,
        show: Option<Box<InterruptedShow>>,
        asked: bool,
        scroll: usize,
        page: Option<usize>,
    },
    /// Reclaim's confirmation: the dry run's outcome once it answered.
    Reclaim {
        id: String,
        dry: Option<ReclaimOutcome>,
    },
    /// Dismiss all's confirmation, naming exactly the items listed when it was
    /// asked, so one that appears meanwhile is not dismissed unseen.
    DismissAll { ids: Vec<String> },
}

/// A daemon answer, delivered to the render thread.
pub(super) enum Answer {
    /// `Ok(None)`: the daemon does not support interrupted work.
    Listed {
        seq: u64,
        result: Result<Option<InterruptedList>, String>,
    },
    Shown {
        id: String,
        result: Result<Option<InterruptedShow>, String>,
    },
    Reclaimed {
        id: String,
        dry_run: bool,
        result: Result<Option<ReclaimOutcome>, String>,
    },
    Dismissed {
        result: Result<Option<DismissOutcome>, String>,
    },
}

/// What the TUI holds of the interrupted work list.
pub(super) struct InterruptedWork {
    /// The daemon process the list came from; every request is pinned to it,
    /// since an item's id belongs to one daemon's state directory.
    instance: Option<String>,
    /// The daemon process the attach fetch was made for.
    fetched_for: Option<String>,
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
    /// A reclaim or dismissal is in flight.
    busy: bool,
    /// The newest list request; an older one's answer is not applied.
    list_seq: u64,
    tx: mpsc::Sender<Answer>,
    rx: mpsc::Receiver<Answer>,
}

impl Default for InterruptedWork {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        InterruptedWork {
            instance: None,
            fetched_for: None,
            list: None,
            unavailable: None,
            selected: 0,
            scroll: 0,
            view: View::List,
            message: None,
            footer_hidden: false,
            busy: false,
            list_seq: 0,
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

    /// Whether the open view asks y/n now: reclaim's confirmation once its dry
    /// run says the worktree would be removed, and dismiss all's.
    fn asks(&self) -> bool {
        match &self.view {
            View::Reclaim { dry: Some(dry), .. } => !self.busy && would_remove(dry),
            View::DismissAll { ids } => !self.busy && !ids.is_empty(),
            _ => false,
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

/// Run `work` on the tokio runtime and deliver its answer to [`drain`].
/// `false`, with nothing run, outside a runtime (the L1 seams, which hand the
/// answers in themselves).
fn spawn(iw: &InterruptedWork, work: impl Future<Output = Answer> + Send + 'static) -> bool {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    let tx = iw.tx.clone();
    handle.spawn(async move {
        let _ = tx.send(work.await);
    });
    true
}

/// Ask for the list again; `git` asks for each item's Git and pull request
/// facts too.
fn request_list(iw: &mut InterruptedWork, git: bool) {
    iw.list_seq += 1;
    let seq = iw.list_seq;
    let instance = iw.instance.clone();
    spawn(iw, async move {
        let result = bounded(
            LIST_TIMEOUT,
            client().list_interrupted_work(git, instance.as_deref()),
        )
        .await;
        Answer::Listed { seq, result }
    });
}

/// PRD #1790 stage 2: fetch the list once for each daemon process the TUI's
/// event stream attaches to — after the TUI attaches, and after it reconnects
/// to another process. What an earlier process said is forgotten then.
pub(super) fn sync(ui: &mut UiState, incarnation: &Incarnation) {
    let Some(process) = incarnation.as_deref() else {
        return;
    };
    let iw = &mut ui.interrupted;
    if iw.fetched_for.as_deref() == Some(process) {
        return;
    }
    let InterruptedWork { tx, rx, .. } = std::mem::take(iw);
    *iw = InterruptedWork {
        fetched_for: Some(process.to_string()),
        instance: Some(process.to_string()),
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

/// Apply one daemon answer.
pub(super) fn apply(ui: &mut UiState, answer: Answer) {
    let iw = &mut ui.interrupted;
    match answer {
        Answer::Listed { seq, result } => {
            if seq != iw.list_seq {
                return;
            }
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
            let count = iw.ordered().len();
            iw.selected = iw.selected.min(count.saturating_sub(1));
            let gone = match &iw.view {
                View::Details { id, .. } | View::Reclaim { id, .. } => iw.item(id).is_none(),
                View::DismissAll { .. } | View::List => false,
            };
            if gone && !iw.busy {
                iw.view = View::List;
            }
        }
        Answer::Shown { id, result } => {
            if let View::Details {
                id: open,
                show,
                page,
                scroll,
                ..
            } = &mut iw.view
                && *open == id
            {
                match result {
                    // A page this view did not ask for (an earlier request
                    // answering late) is not shown.
                    Ok(Some(shown)) if page.is_some_and(|from| from != shown.reports_from) => {}
                    Ok(Some(shown)) => {
                        *show = Some(Box::new(shown));
                        *scroll = 0;
                    }
                    Ok(None) => iw.message = Some(words::UNSUPPORTED.to_string()),
                    Err(message) => iw.message = Some(message),
                }
            }
        }
        Answer::Reclaimed {
            id,
            dry_run: true,
            result,
        } => {
            let View::Reclaim { id: open, dry } = &mut iw.view else {
                return;
            };
            if *open != id {
                return;
            }
            iw.busy = false;
            match result {
                Ok(Some(outcome)) => {
                    *dry = Some(outcome);
                    // A question that just appeared waits for its own frame.
                    ui.confirm_choices_drawn = None;
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
        }
        Answer::Reclaimed {
            dry_run: false,
            result,
            ..
        } => {
            iw.busy = false;
            iw.message = Some(match result {
                Ok(Some(outcome)) => crate::interrupted_cli::reclaim_message(&outcome),
                Ok(None) => words::UNSUPPORTED.to_string(),
                Err(message) => message,
            });
            iw.view = View::List;
            request_list(iw, false);
        }
        Answer::Dismissed { result } => {
            iw.busy = false;
            iw.message = Some(match result {
                Ok(Some(outcome)) => dismiss_message(&outcome),
                Ok(None) => words::UNSUPPORTED.to_string(),
                Err(message) => message,
            });
            iw.view = View::List;
            request_list(iw, false);
        }
    }
}

/// The sentence for a dismissal's outcome.
fn dismiss_message(outcome: &DismissOutcome) -> String {
    if let Some(refusal) = &outcome.refusal {
        return words::refusal_sentence(refusal.reason).to_string();
    }
    if let Some(refused) = outcome.refused.first() {
        return words::refusal_sentence(refused.refusal.reason).to_string();
    }
    if outcome.dismissed.is_empty() && outcome.dismissed_omitted == 0 {
        return words::NOTHING_DISMISSED.to_string();
    }
    words::DISMISSED.to_string()
}

/// PRD #1790 stage 2: open the overlay on the list, and ask for it again with
/// its Git facts. `current` is the event stream's daemon process, for a TUI
/// whose attach fetch has not answered.
pub(super) fn open(ui: &mut UiState, current: &Incarnation) {
    let iw = &mut ui.interrupted;
    if iw.instance.is_none() {
        iw.instance = current.as_deref().map(str::to_string);
    }
    iw.view = View::List;
    iw.message = None;
    ui.mode = UiMode::InterruptedWork;
    request_list(iw, true);
}

/// PRD #1790 stage 2: hide the footer row until another daemon process.
pub(super) fn hide_footer(ui: &mut UiState) {
    ui.interrupted.footer_hidden = true;
}

/// The number the footer row shows, when it is shown.
pub(super) fn footer_count(ui: &UiState) -> Option<usize> {
    let iw = &ui.interrupted;
    if iw.footer_hidden {
        return None;
    }
    listed_count(ui)
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

/// Keys while the overlay is up. Ctrl+C keeps its PRD #40 safety net.
pub(super) fn handle_key(key: KeyEvent, ui: &mut UiState, kb: &KeybindingConfig) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        ui.quit_confirm_selected = 0;
        ui.mode = UiMode::QuitConfirm;
        return Action::Continue;
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
            _ => None,
        },
    };
    press.map_or(Action::Continue, Action::InterruptedWork)
}

/// How far PageUp/PageDown scroll the details.
const DETAILS_PAGE: usize = 10;

/// Do what `press` asks.
pub(super) fn press(ui: &mut UiState, press: Press) {
    if press == Press::Close {
        ui.interrupted.view = View::List;
        if ui.mode == UiMode::InterruptedWork {
            ui.mode = UiMode::Normal;
        }
        return;
    }
    let iw = &mut ui.interrupted;
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
                    };
                }
            }
            Press::Reclaim => {
                let Some(item) = iw.selected_item() else {
                    return;
                };
                // Decision D1: a plain agent has no worktree the deck created.
                if iw.busy || item.kind == ItemKind::Agent {
                    return;
                }
                let id = item.id.as_str().to_string();
                iw.view = View::Reclaim {
                    id: id.clone(),
                    dry: None,
                };
                reclaim(iw, id, true);
            }
            Press::Dismiss => {
                if iw.busy {
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
                if !iw.busy && !ids.is_empty() {
                    iw.view = View::DismissAll { ids };
                    // A question that just appeared waits for its own frame.
                    ui.confirm_choices_drawn = None;
                }
            }
            _ => {}
        },
        View::Details {
            id, asked, show, ..
        } => match press {
            Press::Down | Press::Up | Press::PageDown | Press::PageUp => {
                if let View::Details { scroll, .. } = &mut iw.view {
                    *scroll = match press {
                        Press::Down => scroll.saturating_add(1),
                        Press::Up => scroll.saturating_sub(1),
                        Press::PageDown => scroll.saturating_add(DETAILS_PAGE),
                        _ => scroll.saturating_sub(DETAILS_PAGE),
                    };
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
            Press::Back => iw.view = View::List,
            _ => {}
        },
        View::Reclaim { id, dry } => match press {
            Press::Yes => {
                if !iw.busy && dry.as_ref().is_some_and(would_remove) {
                    reclaim(iw, id, false);
                }
            }
            // A reclaim in flight still reports its sentence in the list.
            Press::Back => iw.view = View::List,
            _ => {}
        },
        View::DismissAll { ids } => match press {
            Press::Yes => {
                if !iw.busy {
                    dismiss(iw, ids);
                }
            }
            Press::Back => iw.view = View::List,
            _ => {}
        },
    }
}

/// Ask for the page of the item `id`'s kept reports, and their text, that
/// starts at its `from`-th report; only that page's answer is shown.
fn request_page(iw: &mut InterruptedWork, id: String, from: usize) {
    if let View::Details { asked, page, .. } = &mut iw.view {
        *asked = true;
        *page = Some(from);
    }
    let instance = iw.instance.clone();
    spawn(iw, async move {
        let result = bounded(
            SHOW_TIMEOUT,
            client().show_interrupted_page(&id, None, Some(from), instance.as_deref()),
        )
        .await;
        Answer::Shown { id, result }
    });
}

/// Reclaim the item `id`, or with `dry_run` ask what that would do.
fn reclaim(iw: &mut InterruptedWork, id: String, dry_run: bool) {
    iw.busy = true;
    let instance = iw.instance.clone();
    if !spawn(iw, async move {
        let result = bounded(
            RECLAIM_TIMEOUT,
            client().reclaim_interrupted(&id, dry_run, None, instance.as_deref()),
        )
        .await;
        Answer::Reclaimed {
            id,
            dry_run,
            result,
        }
    }) {
        iw.busy = false;
    }
}

/// Dismiss the items `ids` name.
fn dismiss(iw: &mut InterruptedWork, ids: Vec<String>) {
    iw.busy = true;
    iw.view = View::List;
    let instance = iw.instance.clone();
    if !spawn(iw, async move {
        let result = bounded(
            DISMISS_TIMEOUT,
            client().dismiss_interrupted(ids, false, None, instance.as_deref()),
        )
        .await;
        Answer::Dismissed { result }
    }) {
        iw.busy = false;
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
    let Some(count) = footer_count(ui) else {
        return 0;
    };
    let sentence = unicode_width::UnicodeWidthStr::width(words::interrupted_footer(count).as_str());
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
    let Some(count) = footer_count(ui) else {
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
            words::interrupted_footer(count),
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
/// there is any.
pub(super) fn restart_notice_hint(ui: &UiState) -> Option<String> {
    let count = listed_count(ui)?;
    Some(format!(
        "  {} · {} to show them",
        words::interrupted_footer(count),
        display_notation(&ui.keybindings, KbAction::InterruptedWork),
    ))
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
    if let Some(git) = &item.git {
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

/// Reclaim's confirmation body, and the row of its y/n question when it asks.
fn reclaim_body(
    iw: &InterruptedWork,
    id: &str,
    dry: Option<&ReclaimOutcome>,
    width: usize,
) -> (Vec<Line<'static>>, Option<(usize, String)>) {
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
        return (lines, None);
    };
    lines.push(Line::from(""));
    lines.extend(wrapped(
        &crate::interrupted_cli::reclaim_message(dry),
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
        return (lines, None);
    }
    lines.push(Line::from(""));
    let question = format!("{}? y/n", words::RECLAIM_ACTION);
    let at = lines.len();
    lines.push(Line::styled(format!("  {question}"), heading()));
    (lines, Some((at, question)))
}

/// The hints under the body.
fn hints(iw: &InterruptedWork) -> String {
    let lower = |word: &str| word.to_lowercase();
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
            format!("  y {} · n back", lower(words::RECLAIM_ACTION))
        }
        View::DismissAll { .. } if iw.asks() => {
            format!("  y {} · n back", lower(words::DISMISS_ALL_ACTION))
        }
        View::Reclaim { .. } | View::DismissAll { .. } => "  Esc back".to_string(),
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
            (body, selected, None)
        }
        View::Details {
            id, show, asked, ..
        } => {
            let item = show.as_ref().map(|s| &s.item).or_else(|| iw.item(id));
            let body = item
                .map(|item| details_body(item, show.as_deref(), *asked, width))
                .unwrap_or_default();
            (body, None, None)
        }
        View::Reclaim { id, dry } => {
            let (body, question) = reclaim_body(iw, id, dry.as_ref(), width);
            (body, None, question)
        }
        View::DismissAll { ids } => {
            let question = words::dismiss_all_question(ids.len())
                .trim_end()
                .to_string();
            let body = vec![Line::styled(format!("  {question}"), heading())];
            let question = iw.asks().then_some((0, question));
            (body, None, question)
        }
    };
    let message: Vec<Line<'static>> = match (&iw.view, &iw.message) {
        (View::List, Some(message)) => wrapped(message, width, "  ", notice()),
        _ => Vec::new(),
    };
    let hint = hints(iw);
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
    // Keep the selected item in view; the details scroll where the keys put
    // them, up to their last screenful.
    let scroll = match &mut iw.view {
        View::List => {
            if let Some((first, last)) = selected_rows {
                if first < iw.scroll {
                    iw.scroll = first;
                } else if last >= iw.scroll + body_height {
                    iw.scroll = last + 1 - body_height.min(last + 1);
                }
            }
            iw.scroll = iw.scroll.min(body.len().saturating_sub(body_height));
            iw.scroll
        }
        View::Details { scroll, .. } => {
            *scroll = (*scroll).min(body.len().saturating_sub(body_height));
            *scroll
        }
        View::Reclaim { .. } | View::DismissAll { .. } => 0,
    };
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
    // flushed frame shows it (`refuse_undrawn_choice`).
    let confirmation = question.map(|(at, text)| {
        let row = (at < body_height)
            .then(|| u16::try_from(at + 1).ok())
            .flatten()
            .and_then(|r| inner.y.checked_add(r));
        ConfirmDrawing {
            buttons: Vec::new(),
            choices: ConfirmChoices {
                inner,
                cells: vec![(inner.x + 2, row, text)],
            },
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

/// An answer the L1 seam hands in, as the request would deliver it.
pub(super) fn seam_answer(ui: &UiState, step: &InterruptedSeamStep) -> Option<Answer> {
    let iw = &ui.interrupted;
    Some(match step {
        InterruptedSeamStep::Key(_) => return None,
        InterruptedSeamStep::Listed(list) => Answer::Listed {
            seq: iw.list_seq,
            result: Ok(Some(list.clone())),
        },
        InterruptedSeamStep::Shown(show) => Answer::Shown {
            id: show.item.id.as_str().to_string(),
            result: Ok(Some(show.clone())),
        },
        InterruptedSeamStep::Reclaimed(outcome) => Answer::Reclaimed {
            id: match &iw.view {
                View::Reclaim { id, .. } => id.clone(),
                _ => outcome.id.clone().unwrap_or_default(),
            },
            dry_run: outcome.dry_run,
            result: Ok(Some(outcome.clone())),
        },
        InterruptedSeamStep::Dismissed(outcome) => Answer::Dismissed {
            result: Ok(Some(outcome.clone())),
        },
    })
}

/// The L1 seam's start: the attach fetch answered `list`.
pub(super) fn seam_listed(ui: &mut UiState, list: &InterruptedList) {
    let answer = Answer::Listed {
        seq: ui.interrupted.list_seq,
        result: Ok(Some(list.clone())),
    };
    apply(ui, answer);
}
