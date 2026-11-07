//! PRD #1631 (audit A8) — the lease a voice answer is sent under.
//!
//! The webview's countdown ends, the panel calls `desktop_voice_answer_question`,
//! and Rust then reads the deck's snapshot and takes the deck's link before the
//! request is written — each an await that can stall. The webview's own epoch
//! check runs only after that call returns, so on its own it can suppress a
//! report but never the send. So the panel names a lease when it sends, and
//! cancels it — synchronously, from its side — the moment the answer stops
//! being wanted: Cancel pressed, the pane, the deck or the terminal's
//! visibility changed, or voice turned off. Rust holds the request to the lease
//! at its last gate, `DaemonClient::answer_question_while`'s `still_wanted`,
//! immediately before the frame is written.
//!
//! A cancel can reach Rust before the send it cancels does (two IPC calls,
//! each spawned on its own), so a cancel for a lease no send has begun is kept
//! as a tombstone, and the send that names it later begins already dead.
//!
//! Those tombstones are bounded, and forgetting one must not let its send
//! through (PR #1698 review). So a lease is named by a number the panel only
//! ever raises — the clock's microseconds when it sends, or one more than its
//! last lease when that is higher — and when a tombstone has to go, the number
//! it forgot becomes a floor that no lease at or below it begins under alive.
//! The panel's next send is numbered above every lease it already named, so
//! the floor refuses a send whose cancel may have been forgotten, and none it
//! has yet to make. Nothing is cleared when the page reloads, so a late send
//! from the page before still finds its cancel.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use dot_agent_deck::daemon_client::AnswerReport;

/// Longest lease name accepted: the digits of a `u64`.
pub const MAX_LEASE_CHARS: usize = 20;

/// Cancels kept for sends that have not begun. One panel has at most one
/// answer in flight, so this only bounds a webview that cancels names it
/// never sends; one past it raises the floor instead.
const MAX_TOMBSTONES: usize = 32;

/// Whether `lease` may name a lease: a decimal number of 1 to
/// [`MAX_LEASE_CHARS`] digits that fits a `u64`.
pub fn is_valid_lease(lease: &str) -> bool {
    parse(lease).is_some()
}

/// The number `lease` names.
fn parse(lease: &str) -> Option<u64> {
    if lease.is_empty()
        || lease.len() > MAX_LEASE_CHARS
        || !lease.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    lease.parse().ok()
}

/// Every live lease, and the cancels that arrived before their send.
#[derive(Default)]
pub struct AnswerLeases {
    book: Arc<Mutex<Book>>,
}

#[derive(Default)]
struct Book {
    live: std::collections::HashMap<u64, Arc<AtomicBool>>,
    /// Leases cancelled before their send began, each above `floor`.
    tombstones: BTreeSet<u64>,
    /// No lease at or below this begins alive: a cancel for one of them may
    /// have been forgotten.
    floor: u64,
}

/// One send's lease. Dropping it forgets the lease.
pub struct AnswerLease {
    /// `None` for a name [`is_valid_lease`] refuses, which begins dead.
    number: Option<u64>,
    alive: Arc<AtomicBool>,
    book: Arc<Mutex<Book>>,
}

impl AnswerLeases {
    /// Begin the send named `lease`. Dead from the start when the panel
    /// already cancelled it, when its cancel may have been forgotten (it is at
    /// or below the floor), or when it is not a name [`is_valid_lease`]
    /// accepts.
    pub fn begin(&self, lease: &str) -> AnswerLease {
        let mut book = self.book.lock().unwrap();
        let number = parse(lease);
        let alive = number.is_some_and(|n| !book.tombstones.remove(&n) && n > book.floor);
        let alive = Arc::new(AtomicBool::new(alive));
        if let Some(n) = number {
            book.live.insert(n, Arc::clone(&alive));
        }
        AnswerLease {
            number,
            alive,
            book: Arc::clone(&self.book),
        }
    }

    /// The panel no longer wants the answer sent under `lease`.
    pub fn cancel(&self, lease: &str) {
        let Some(number) = parse(lease) else {
            return;
        };
        let mut book = self.book.lock().unwrap();
        if let Some(alive) = book.live.get(&number) {
            alive.store(false, Ordering::SeqCst);
            return;
        }
        if number <= book.floor {
            return;
        }
        book.tombstones.insert(number);
        if book.tombstones.len() > MAX_TOMBSTONES
            && let Some(oldest) = book.tombstones.pop_first()
        {
            // Every tombstone is above the floor, so this only raises it.
            book.floor = oldest;
        }
    }
}

impl AnswerLease {
    /// Whether the panel still wants this answer sent.
    pub fn is_live(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// The `still_wanted` gate for `DaemonClient::answer_question_while`.
    pub fn still_wanted(&self) -> impl Fn() -> bool + Send + Sync + 'static {
        let alive = Arc::clone(&self.alive);
        move || alive.load(Ordering::SeqCst)
    }
}

impl Drop for AnswerLease {
    fn drop(&mut self) {
        let Some(number) = self.number else {
            return;
        };
        let mut book = self.book.lock().unwrap();
        if book
            .live
            .get(&number)
            .is_some_and(|alive| Arc::ptr_eq(alive, &self.alive))
        {
            book.live.remove(&number);
        }
    }
}

/// How a leased send ended, as far as the lease is concerned.
#[derive(Debug, PartialEq, Eq)]
pub enum Leased {
    /// The lease was cancelled before the request was written: nothing went.
    Cancelled,
    /// The lease was cancelled after the request was written: the answer went
    /// anyway, and the user must be told so rather than "nothing was sent".
    TooLate(AnswerReport),
    /// The lease held throughout.
    Sent(AnswerReport),
    /// The send failed — it timed out, or the connection broke — AFTER the
    /// gate let the request through, so it may have been written and acted
    /// on: the outcome is unknown, never "nothing was sent". Carries the
    /// failure, for the log.
    Unconfirmed(String),
}

/// Run `send` under `lease`. `send` gets the lease's `still_wanted` gate to
/// pass to `DaemonClient::answer_question_while`; a `Superseded` report while
/// the lease is dead is the gate having held the frame back. A failure before
/// the gate let the request through is returned as the error it is — nothing
/// was sent; one after is [`Leased::Unconfirmed`].
pub async fn send_leased<F>(
    lease: &AnswerLease,
    send: impl FnOnce(Box<dyn Fn() -> bool + Send + Sync>) -> F,
) -> Result<Leased, String>
where
    F: std::future::Future<Output = Result<AnswerReport, String>>,
{
    if !lease.is_live() {
        return Ok(Leased::Cancelled);
    }
    let passed = Arc::new(AtomicBool::new(false));
    let gate = {
        let still_wanted = lease.still_wanted();
        let passed = Arc::clone(&passed);
        move || {
            let wanted = still_wanted();
            if wanted {
                passed.store(true, Ordering::SeqCst);
            }
            wanted
        }
    };
    let report = match send(Box::new(gate)).await {
        Ok(report) => report,
        Err(failure) if passed.load(Ordering::SeqCst) => return Ok(Leased::Unconfirmed(failure)),
        Err(failure) => return Err(failure),
    };
    Ok(match report {
        AnswerReport::Superseded | AnswerReport::Withheld if !lease.is_live() => Leased::Cancelled,
        AnswerReport::Answered | AnswerReport::Refused(_) if !lease.is_live() => {
            Leased::TooLate(report)
        }
        report => Leased::Sent(report),
    })
}
