//! The deck lists settings saves wrote, in the order they wrote them, and the
//! order they are put into force in (issue #1620).
//!
//! # The race this closes
//!
//! Putting a saved deck list into force (`retarget_to_published` in `lib.rs`)
//! awaits: it detaches the terminals of decks that left the fleet, which writes
//! a frame to each, and only then releases those decks' transports and ends
//! their watchers. Two saves made in quick succession used to run that work
//! side by side, so an older save paused in its detach could resume after a
//! newer save had finished and release the transports and watchers of decks
//! the newer save had kept — from a deck list that was no longer on disk.
//!
//! # Publish in write order, apply the newest
//!
//! A save **publishes** the settings it wrote while it still holds the order
//! its write was made in, so the newest ticket is always the newest write. The
//! deck work then runs one pass at a time behind [`SavedSelections::in_order`],
//! and each pass applies the **newest** published settings rather than the
//! ones its own save wrote: an older save that reaches the order late finds a
//! newer list already in force, or applies that newer list itself, rather than
//! its own. A pass that sees a newer ticket arrive while it is detaching
//! ([`SavedSelections::superseded`]) stops before the rest of its teardown, and
//! the next pass applies the newer list.
//!
//! Only the deck work waits here. The settings write, and the voice settings
//! a save puts in force before its deck work starts, do not.

use std::sync::Mutex;

use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

use crate::settings::DesktopSettings;

#[derive(Default)]
pub(crate) struct SavedSelections {
    /// The newest published settings and their ticket. Tickets start at 1, so
    /// an applied ticket of 0 means nothing published has been applied yet.
    latest: Mutex<Option<(u64, DesktopSettings)>>,
    /// Held for the whole of a deck pass. The value is the ticket of the
    /// settings last put fully into force.
    applied: AsyncMutex<u64>,
}

impl SavedSelections {
    /// Record `settings` as the newest saved deck list, and return its ticket.
    ///
    /// Call it while the save that wrote `settings` still holds the order its
    /// write was made in, so tickets follow the writes.
    pub(crate) fn publish(&self, settings: &DesktopSettings) -> u64 {
        let mut latest = self.latest();
        let ticket = latest.as_ref().map_or(0, |(ticket, _)| *ticket) + 1;
        *latest = Some((ticket, settings.clone()));
        ticket
    }

    /// Wait for the deck passes before this one to finish. The guard holds the
    /// ticket last put into force; the pass updates it when it completes.
    pub(crate) async fn in_order(&self) -> AsyncMutexGuard<'_, u64> {
        self.applied.lock().await
    }

    /// The newest published settings, if they are newer than `applied`.
    pub(crate) fn newer_than(&self, applied: u64) -> Option<(u64, DesktopSettings)> {
        self.latest()
            .as_ref()
            .filter(|(ticket, _)| *ticket > applied)
            .cloned()
    }

    /// Whether settings newer than `ticket` have been published since.
    pub(crate) fn superseded(&self, ticket: u64) -> bool {
        self.latest()
            .as_ref()
            .is_some_and(|(newest, _)| *newest > ticket)
    }

    /// Poison-tolerant: a ticket and a settings value cannot be left
    /// half-written by a panic elsewhere.
    fn latest(&self) -> std::sync::MutexGuard<'_, Option<(u64, DesktopSettings)>> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_follow_publishes_and_only_the_newest_is_offered() {
        let saves = SavedSelections::default();
        assert!(saves.newer_than(0).is_none(), "nothing published yet");

        let first = saves.publish(&DesktopSettings::default());
        let second = saves.publish(&DesktopSettings::default());
        assert!(second > first);

        assert_eq!(saves.newer_than(0).map(|(ticket, _)| ticket), Some(second));
        assert_eq!(
            saves.newer_than(first).map(|(ticket, _)| ticket),
            Some(second)
        );
        assert!(saves.newer_than(second).is_none(), "the newest is in force");

        assert!(saves.superseded(first));
        assert!(!saves.superseded(second));
    }
}
