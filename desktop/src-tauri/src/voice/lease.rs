//! PRD #1542 (audit A8) — the lease a voice answer is sent under.
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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use dot_agent_deck::daemon_client::AnswerReport;

/// Longest lease name accepted.
pub const MAX_LEASE_CHARS: usize = 64;

/// Cancels kept for sends that have not begun. One panel has at most one
/// answer in flight, so this only bounds a webview that cancels names it
/// never sends.
const MAX_TOMBSTONES: usize = 32;

/// Whether `lease` may name a lease: 1 to [`MAX_LEASE_CHARS`] of
/// `[A-Za-z0-9-]`.
pub fn is_valid_lease(lease: &str) -> bool {
    !lease.is_empty()
        && lease.len() <= MAX_LEASE_CHARS
        && lease
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Every live lease, and the cancels that arrived before their send.
#[derive(Default)]
pub struct AnswerLeases {
    book: Arc<Mutex<Book>>,
}

#[derive(Default)]
struct Book {
    live: HashMap<String, Arc<AtomicBool>>,
    tombstones: VecDeque<String>,
}

/// One send's lease. Dropping it forgets the lease.
pub struct AnswerLease {
    name: String,
    alive: Arc<AtomicBool>,
    book: Arc<Mutex<Book>>,
}

impl AnswerLeases {
    /// Begin the send named `lease`. Dead from the start when the panel
    /// already cancelled it.
    pub fn begin(&self, lease: &str) -> AnswerLease {
        let mut book = self.book.lock().unwrap();
        let cancelled = match book.tombstones.iter().position(|name| name == lease) {
            Some(at) => {
                book.tombstones.remove(at);
                true
            }
            None => false,
        };
        let alive = Arc::new(AtomicBool::new(!cancelled));
        book.live.insert(lease.to_string(), Arc::clone(&alive));
        AnswerLease {
            name: lease.to_string(),
            alive,
            book: Arc::clone(&self.book),
        }
    }

    /// The panel no longer wants the answer sent under `lease`.
    pub fn cancel(&self, lease: &str) {
        let mut book = self.book.lock().unwrap();
        match book.live.get(lease) {
            Some(alive) => alive.store(false, Ordering::SeqCst),
            None => {
                if book.tombstones.len() >= MAX_TOMBSTONES {
                    book.tombstones.pop_front();
                }
                book.tombstones.push_back(lease.to_string());
            }
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
        let mut book = self.book.lock().unwrap();
        if book
            .live
            .get(&self.name)
            .is_some_and(|alive| Arc::ptr_eq(alive, &self.alive))
        {
            book.live.remove(&self.name);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario (question/desktop/011, audit A8): the countdown ends and the
    /// answer's send stalls taking the deck's link. Meanwhile the user changes
    /// pane, which cancels the lease; when the stall clears, the last gate
    /// before the frame finds the lease dead and nothing is written. A cancel
    /// that reaches Rust before its send does still kills it, and a cancel that
    /// lands after the frame was written is reported as too late, not as
    /// nothing sent. A send that times out after the gate let the frame
    /// through is reported unconfirmed — it may have been sent — and one that
    /// fails before the gate is the plain error.
    #[tokio::test]
    async fn question_desktop_011_a_cancelled_lease_writes_no_frame() {
        let leases = AnswerLeases::default();
        let lease = leases.begin("lease-1");
        let (stall_tx, stall_rx) = tokio::sync::oneshot::channel::<()>();
        let written = Arc::new(AtomicBool::new(false));
        let sending = {
            let written = Arc::clone(&written);
            send_leased(&lease, move |still_wanted| async move {
                // The snapshot read and the link, stalled.
                let _ = stall_rx.await;
                // `answer_question_while`'s last gate, right before the write.
                if !still_wanted() {
                    return Ok(AnswerReport::Superseded);
                }
                written.store(true, Ordering::SeqCst);
                Ok(AnswerReport::Answered)
            })
        };
        let canceller = async {
            tokio::task::yield_now().await;
            leases.cancel("lease-1");
            let _ = stall_tx.send(());
        };
        let (outcome, ()) = tokio::join!(sending, canceller);
        assert_eq!(outcome, Ok(Leased::Cancelled));
        assert!(!written.load(Ordering::SeqCst), "no frame was written");

        // A cancel that arrives before its send.
        leases.cancel("lease-2");
        let early = leases.begin("lease-2");
        assert!(!early.is_live());
        let outcome = send_leased(&early, |_| async {
            panic!("a dead lease never sends");
            #[allow(unreachable_code)]
            Ok(AnswerReport::Answered)
        })
        .await;
        assert_eq!(outcome, Ok(Leased::Cancelled));

        // A cancel after the frame went: too late, and said so.
        let late = leases.begin("lease-3");
        let outcome = send_leased(&late, |still_wanted| {
            let leases = &leases;
            async move {
                assert!(still_wanted());
                leases.cancel("lease-3");
                Ok(AnswerReport::Answered)
            }
        })
        .await;
        assert_eq!(outcome, Ok(Leased::TooLate(AnswerReport::Answered)));

        // A lease that held throughout.
        let held = leases.begin("lease-4");
        let outcome = send_leased(&held, |_| async { Ok(AnswerReport::Answered) }).await;
        assert_eq!(outcome, Ok(Leased::Sent(AnswerReport::Answered)));

        // A send that fails after the gate let the request through may have
        // been acted on: unconfirmed, not an error saying nothing went. One
        // that fails before the gate is the plain error.
        let timed_out = leases.begin("lease-5");
        let outcome = send_leased(&timed_out, |still_wanted| async move {
            assert!(still_wanted());
            Err("the deck did not answer in time".to_string())
        })
        .await;
        assert_eq!(
            outcome,
            Ok(Leased::Unconfirmed(
                "the deck did not answer in time".to_string()
            ))
        );
        let unreached = leases.begin("lease-6");
        let outcome = send_leased(&unreached, |_| async {
            Err("the deck did not answer in time".to_string())
        })
        .await;
        assert_eq!(outcome, Err("the deck did not answer in time".to_string()));

        assert!(is_valid_lease("a1-b2"));
        assert!(!is_valid_lease(""));
        assert!(!is_valid_lease("a b"));
        assert!(!is_valid_lease(&"a".repeat(MAX_LEASE_CHARS + 1)));
    }
}
