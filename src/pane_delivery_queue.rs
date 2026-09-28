//! Issue #544 (PR #1398 review): the daemon hook loop's automatic deliveries,
//! written one pane at a time in the order they were queued, with a bound on
//! how many can be pending at once.
//!
//! A `work-done` hand-off and a `dispatch` result are written into their
//! recipient's pane as automatic FIRST writes, which wait while that pane holds
//! the user's unsent draft ([`crate::draft_deferral`]). Awaited inside the hook
//! connection, a wait holds one of the daemon's
//! [`crate::daemon::MAX_CONCURRENT_HOOK_CONNECTIONS`] permits for up to the
//! draft cap, so they are handed off instead. Handed off as one detached task
//! each, two problems followed: nothing bounded how many such tasks could
//! exist, and several of them deferred on one pane's draft each polled on its
//! own, so the draft's release could write them in any order — a later report
//! arriving before an earlier one.
//!
//! So each target pane gets one FIFO queue and at most one task draining it,
//! which exists only while that queue holds work: it is started by the send
//! that finds no queue, and it removes its queue and exits when it finds the
//! queue empty. Both decisions are taken under one mutex, so a send can never
//! land in a queue whose task has already decided to exit. Across panes the
//! deliveries still run concurrently, so one pane's draft delays nothing
//! written anywhere else.
//!
//! The bound is on deliveries pending across all panes,
//! [`MAX_PENDING_PANE_DELIVERIES`], and therefore on drain tasks too (a task
//! exists only for a pane with at least one pending delivery). At the bound
//! the enqueuing hook connection WAITS for a slot, holding its connection
//! permit — backpressure onto new hook connections, logged once per episode,
//! and never a dropped delivery.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

/// Issue #544: how many hook-loop deliveries may be pending — queued or being
/// written — across all panes before a new one makes its hook connection wait.
///
/// Eight times [`crate::daemon::MAX_CONCURRENT_HOOK_CONNECTIONS`]: a pending
/// delivery is one small composed message, and each is normally pending only
/// while its recipient's draft is, so reaching this means hundreds of reports
/// stacked up behind drafts nobody is finishing. Up to the cap, every pane's
/// queue drains within one draft cap of its draft being released or of the
/// cap running out.
pub const MAX_PENDING_PANE_DELIVERIES: usize = 256;

/// One queued delivery: a future that performs the write and logs its own
/// outcome. Inert until the pane's drain task polls it.
pub type PaneDelivery = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

type Queued = (PaneDelivery, OwnedSemaphorePermit);

/// Issue #544: see the module docs.
pub struct PaneDeliveryQueues {
    queues: Mutex<HashMap<String, mpsc::UnboundedSender<Queued>>>,
    pending: Arc<Semaphore>,
    /// Whether the bound is currently holding, so the warning fires on the
    /// transition into saturation rather than once per waiting delivery.
    at_cap: AtomicBool,
}

impl PaneDeliveryQueues {
    pub fn new() -> Arc<Self> {
        Self::with_capacity(MAX_PENDING_PANE_DELIVERIES)
    }

    /// [`Self::new`] with a different bound; the seam the tests use to reach
    /// it without queueing hundreds of deliveries.
    pub fn with_capacity(max_pending: usize) -> Arc<Self> {
        Arc::new(Self {
            queues: Mutex::new(HashMap::new()),
            pending: Arc::new(Semaphore::new(max_pending)),
            at_cap: AtomicBool::new(false),
        })
    }

    /// Queue `delivery` behind every delivery already queued for `pane_id`.
    ///
    /// Returns once it is queued — at once, unless
    /// [`MAX_PENDING_PANE_DELIVERIES`] are already pending, in which case it
    /// first waits for one of them to finish. It never waits for `delivery`
    /// itself.
    pub async fn enqueue(self: &Arc<Self>, pane_id: &str, delivery: PaneDelivery) {
        let permit = match Arc::clone(&self.pending).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if !self.at_cap.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        pane_id = %pane_id,
                        max_pending = MAX_PENDING_PANE_DELIVERIES,
                        "hook-loop pane deliveries are at their bound (recipients holding unsent \
                         drafts); new ones wait for a slot and hold their hook connection meanwhile"
                    );
                }
                let permit = Arc::clone(&self.pending)
                    .acquire_owned()
                    .await
                    .expect("the pending-delivery semaphore is never closed");
                self.at_cap.store(false, Ordering::Relaxed);
                permit
            }
        };
        let mut queues = self.queues.lock().unwrap();
        let mut queued = (delivery, permit);
        if let Some(tx) = queues.get(pane_id) {
            match tx.send(queued) {
                Ok(()) => return,
                // The drain task is gone without removing its queue, which only
                // a panic inside a delivery can do. Start a fresh one.
                Err(mpsc::error::SendError(returned)) => queued = returned,
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(queued)
            .expect("the receiver was created on the line above");
        queues.insert(pane_id.to_string(), tx);
        drop(queues);
        tokio::spawn(Arc::clone(self).drain(pane_id.to_string(), rx));
    }

    async fn drain(self: Arc<Self>, pane_id: String, mut rx: mpsc::UnboundedReceiver<Queued>) {
        loop {
            let next = {
                let mut queues = self.queues.lock().unwrap();
                match rx.try_recv() {
                    Ok(queued) => Some(queued),
                    Err(_) => {
                        // Empty, under the lock every send takes: nothing can
                        // be queued here after this, so the next send for this
                        // pane starts a queue and a task of its own.
                        queues.remove(&pane_id);
                        None
                    }
                }
            };
            let Some((delivery, _permit)) = next else {
                return;
            };
            delivery.await;
        }
    }

    #[cfg(test)]
    fn queued_panes(&self) -> usize {
        self.queues.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn recording(log: &Arc<Mutex<Vec<&'static str>>>, name: &'static str) -> PaneDelivery {
        let log = Arc::clone(log);
        Box::pin(async move { log.lock().unwrap().push(name) })
    }

    fn held(
        log: &Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
    ) -> (PaneDelivery, oneshot::Sender<()>) {
        let (release, released) = oneshot::channel();
        let log = Arc::clone(log);
        let delivery = Box::pin(async move {
            let _ = released.await;
            log.lock().unwrap().push(name);
        });
        (delivery, release)
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn deliveries_to_one_pane_run_in_the_order_they_were_queued() {
        let queues = PaneDeliveryQueues::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (first, release_first) = held(&log, "first");
        queues.enqueue("orch", first).await;
        queues.enqueue("orch", recording(&log, "second")).await;
        queues.enqueue("orch", recording(&log, "third")).await;
        settle().await;
        assert!(
            log.lock().unwrap().is_empty(),
            "a later delivery ran while the first was still waiting"
        );
        release_first.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while log.lock().unwrap().len() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the queue drained");
        assert_eq!(*log.lock().unwrap(), vec!["first", "second", "third"]);
    }

    #[tokio::test]
    async fn one_panes_wait_does_not_hold_another_pane() {
        let queues = PaneDeliveryQueues::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (waiting, _release) = held(&log, "waiting");
        queues.enqueue("orch-a", waiting).await;
        queues
            .enqueue("orch-b", recording(&log, "other-pane"))
            .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while log.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the other pane's delivery ran");
        assert_eq!(*log.lock().unwrap(), vec!["other-pane"]);
    }

    #[tokio::test]
    async fn at_the_bound_enqueue_waits_and_drops_nothing() {
        let queues = PaneDeliveryQueues::with_capacity(1);
        let log = Arc::new(Mutex::new(Vec::new()));
        let (first, release_first) = held(&log, "first");
        queues.enqueue("orch", first).await;
        let second = {
            let queues = Arc::clone(&queues);
            let delivery = recording(&log, "second");
            tokio::spawn(async move { queues.enqueue("other", delivery).await })
        };
        settle().await;
        assert!(
            !second.is_finished(),
            "a delivery past the bound was queued without waiting for a slot"
        );
        release_first.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the waiting enqueue got a slot")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while log.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the waiting delivery ran");
        assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);
    }

    #[tokio::test]
    async fn a_drained_queue_and_its_task_go_away_and_a_new_one_starts_later() {
        let queues = PaneDeliveryQueues::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        queues.enqueue("orch", recording(&log, "one")).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while queues.queued_panes() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the idle queue was removed");
        queues.enqueue("orch", recording(&log, "two")).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while log.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a later delivery to the same pane still ran");
        assert_eq!(*log.lock().unwrap(), vec!["one", "two"]);
    }
}
