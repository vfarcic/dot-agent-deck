//! PRD #742 M8 — one answer to "did the thing I am publishing into still want
//! me?", for the three places that asked it separately.
//!
//! # The question
//!
//! Three defects found on this branch are the same defect. Each is a *claim
//! staked before an await and honoured after it*, with no way to tell that the
//! world moved in between:
//!
//! - [`crate::endpoint_tunnels::EndpointTunnels::acquire`] checks the map,
//!   releases the lock, spawns `ssh` (up to 30 s), and re-acquires the lock to
//!   publish a lease. A `retain` that landed in that window removed nothing —
//!   the entry was not there yet — and the publish then re-created a deck the
//!   user had just dropped, holding an authenticated `ssh` child to a host they
//!   believe they disconnected from.
//! - [`crate::daemon_bridge::DaemonLinks::trusted`] has the identical shape
//!   around `establish()`, with `invalidate_all` as the removal.
//! - [`crate::terminal::DesktopState::register_watcher`] takes a claim slot,
//!   spawns a task, and hands the handle back — and could not tell its own
//!   claim from a slot a *later* claim had re-created, so one watcher's handle
//!   landed in another's slot and was then dropped rather than aborted, leaving
//!   a task nothing could stop.
//!
//! Before PRD #742 M3 the first two were prevented by one whole-map lock held
//! *across establishment*. Splitting that lock per deck (M3) kept the half that
//! collapses concurrent first-uses of one deck and silently gave up the half
//! that excluded teardown. This type is that half, put back without the lock.
//!
//! # The mechanism
//!
//! A monotonic counter. Read it before the await, compare after: **a value that
//! moved means somebody removed something, so do not publish.** Each `bump`
//! also returns a value no other `bump` on the same counter returns, which is
//! what makes the same counter serve as a claim token — the watcher registry
//! needs "is this slot still mine" rather than "did anything change", and those
//! are the same question asked of a monotonic value.
//!
//! # What it deliberately does not do
//!
//! It does not say *which* deck departed, so a teardown that keeps the deck
//! being established still costs that establishment its cache entry: the lease
//! is handed to the caller and dies with the request instead of being published,
//! and the next call establishes again. The alternative — testing membership
//! against `crate::dto::deck_is_observed` — depends on the applied selection
//! having been written before the teardown ran, which is an ordering invariant
//! between two modules rather than a property of this one. The conservative
//! answer costs one re-establishment on a settings save that happened to land
//! inside a connect; the precise answer costs a cross-module invariant that
//! nothing would fail if it broke.
//!
//! # Ordering
//!
//! `bump` is `AcqRel` and `current` is `Acquire`, but the counter is not what
//! makes a compare *exact* — the lock is. A publisher compares under the same
//! map lock it inserts under; where the teardown it is racing bumps under that
//! same lock, the two are serialised and the outcome is exact: either the
//! teardown bumped first and the publisher skips, or the publisher inserted
//! first and the teardown removes what it inserted. Where a teardown bumps under
//! a *different* map's lock the compare is conservative rather than exact, which
//! is the case for `DaemonLinks` against `EndpointTunnels`' own teardowns — that
//! type's *Publishing* section says why it does not need to be exact there.
//! The atomic's own ordering only matters for
//! the read taken *before* the await, which is deliberately unlocked and
//! deliberately conservative — reading a stale (lower) value can only make the
//! later compare see a change that has already been accounted for, which skips a
//! publish that would have been fine. It can never miss one.

use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonic counter for "has this changed since I looked?".
///
/// See the module docs for what it is for and what it costs.
#[derive(Debug, Default)]
pub(crate) struct Generation(AtomicU64);

impl Generation {
    /// The generation in force.
    ///
    /// Taken before an await by a caller that intends to publish, and taken
    /// again under the publishing lock to decide whether it still may.
    pub(crate) fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Move to the next generation, and answer with it.
    ///
    /// Called by every teardown that can make an in-flight establishment
    /// unwanted. The returned value is unique to this call, which is how a
    /// caller that needs a *claim token* rather than an epoch uses the same
    /// counter — see [`crate::terminal::DesktopState::start_watcher_once_for`].
    pub(crate) fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: a fresh counter is read, bumped, and read again. The second
    /// read differs from the first, which is the whole of what a publisher
    /// compares — it never reads the value for anything but inequality.
    #[test]
    fn a_bump_changes_what_current_answers() {
        let generation = Generation::default();
        let before = generation.current();
        generation.bump();
        assert_ne!(
            before,
            generation.current(),
            "a bumped generation must not compare equal to the one read before it"
        );
    }

    /// Scenario: bump the same counter a thousand times and collect every value
    /// it hands back. All thousand are distinct, which is the property the
    /// watcher registry keys a claim on — a token that repeated would let a
    /// later claim's slot accept an earlier claim's handle, which is exactly
    /// the leak the token exists to stop.
    #[test]
    fn every_bump_answers_a_value_no_other_bump_answers() {
        let generation = Generation::default();
        let minted: std::collections::HashSet<u64> = (0..1000).map(|_| generation.bump()).collect();
        assert_eq!(
            minted.len(),
            1000,
            "every bump must mint a token no other bump minted"
        );
    }

    /// Scenario: two threads bump the same counter 5000 times each. Every value
    /// handed out across both threads is distinct and the counter lands at
    /// exactly 10000 — so the token property survives the concurrency it exists
    /// for, rather than holding only on one thread.
    #[test]
    fn concurrent_bumps_still_mint_distinct_tokens() {
        let generation = std::sync::Arc::new(Generation::default());
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let generation = std::sync::Arc::clone(&generation);
                std::thread::spawn(move || {
                    (0..5000).map(|_| generation.bump()).collect::<Vec<u64>>()
                })
            })
            .collect();
        let minted: std::collections::HashSet<u64> = threads
            .into_iter()
            .flat_map(|thread| thread.join().expect("bump thread panicked"))
            .collect();
        assert_eq!(minted.len(), 10_000, "a bump was handed out twice");
        assert_eq!(generation.current(), 10_000);
    }
}
