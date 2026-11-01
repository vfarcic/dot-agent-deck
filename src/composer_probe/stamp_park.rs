//! Test seam (Q84-3): park a real feeder at a named point between taking
//! its timestamp and publishing it, so a test can run a racing caller in
//! that window and then let the feeder go on with its earlier stamp.
//!
//! The module is compiled only under `cfg(test)`, and so is every call to
//! [`StampPark::at`] in the feeders (`OutputHook::on_output`, the observer's
//! marker path): a build without `test` has no park, no field holding one and
//! no call. A park that nothing set costs a test build one uncontended lock
//! per point.

use std::sync::{Arc, Mutex, PoisonError};

/// Where a feeder parks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampPoint {
    /// `OutputHook::on_output`: the push's offset advanced, its stamp not
    /// yet taken.
    OutputAdvanced,
    /// `OutputHook::on_output`: the stamp taken, the push's notes not yet
    /// handed to the generation.
    OutputStamped,
    /// `OutputHook::on_output`: the notes handed over, the return markers
    /// not yet.
    OutputNoted,
    /// The observer's marker path: the stamp taken, neither the observer's
    /// lock nor the generation's taken yet. Linux only, as the observer is.
    #[cfg(target_os = "linux")]
    MarkerStamped,
}

type Park = Arc<dyn Fn(StampPoint) + Send + Sync>;

/// A feeder's park, empty until a test sets one.
#[derive(Default)]
pub struct StampPark(Mutex<Option<Park>>);

impl std::fmt::Debug for StampPark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StampPark")
            .field(&self.lock().is_some())
            .finish()
    }
}

impl StampPark {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Park>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Call `park` at every point a feeder reaches from now on.
    pub fn set(&self, park: impl Fn(StampPoint) + Send + Sync + 'static) {
        *self.lock() = Some(Arc::new(park));
    }

    /// The feeder's side: call the park, if one is set, with its own lock
    /// released, so a parked feeder holds nothing of the park's.
    pub fn at(&self, point: StampPoint) {
        let park = self.lock().clone();
        if let Some(park) = park {
            park(point);
        }
    }
}

/// A park that blocks the feeder at its first arrival at each of `points`
/// until the test sends on the returned `go`, telling the test which point it
/// reached on the returned receiver. Every other point, and every later
/// arrival, passes.
pub fn park_at(
    points: &[StampPoint],
) -> (
    impl Fn(StampPoint) + Send + Sync + 'static,
    std::sync::mpsc::Receiver<StampPoint>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached_tx, reached) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    let pending = Mutex::new((points.to_vec(), reached_tx, go_rx));
    let park = move |at: StampPoint| {
        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
        let (points, reached, go) = &mut *pending;
        if let Some(i) = points.iter().position(|p| *p == at) {
            points.remove(i);
            let _ = reached.send(at);
            let _ = go.recv();
        }
    };
    (park, reached, go)
}
