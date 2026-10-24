//! PRD #1666 part 3, round 5: the inbox hub's half of a dispatched unit's
//! lifecycle (plan D7, D12, D13) — publications, route registration, route
//! termination with its `dispatch_ended` outcome, and a respawned unit
//! terminal's replacement.
//!
//! Every function here is one R2 → R3 linearization section: the registry
//! holds its dispatch-return routes (R2) and hands the guard in, and the
//! section takes the hub (R3) under it and runs as one
//! [`crate::guarded::Guarded`] section over the report state. Reads of the
//! registry's records happen through [`RecordView`] under the hub's lock (R3 →
//! R4), as the hub's live check always has. Nothing here does IO. What a
//! section owes once every lock is released — wakes and owed notices — comes
//! back as [`AfterSection`].
//!
//! When the report state is torn ([`crate::guarded::Torn`]), each of these
//! existing paths does what it did before round 5 and logs once (plan D15
//! rule 4).

use super::*;
use crate::dispatch_return::{DispatchCaller, DispatchReturns, RetainedReturn};
use crate::lock_rank::{R2, RankedGuard};
use crate::report_recovery::{
    AcceptedReport, DispatchPhase, EndedHow, OutcomeCause, OutcomeParty, OutcomePhase,
    OutcomeStorage, OutcomeUnit, Publication, PublishKey, ReportProvenance, ReportState, UnitEntry,
};

/// The registry's dispatch-return routes, held (rank R2).
pub(crate) type ReturnsGuard<'a> = RankedGuard<'a, { R2 }, DispatchReturns>;

/// What a section reads and writes of the registry's records, under the
/// hub's lock (R3 → R4).
pub(crate) trait RecordView {
    /// Whether `agent_id` still has a record or a reservation, so its token
    /// can attest.
    fn registered(&self, agent_id: &str) -> bool;
    /// Pin `unit_id` on `agent_id`'s record or reservation, once. With
    /// `ticket`, only on a generation reserved under that replacement ticket.
    fn pin_unit(&self, agent_id: &str, unit_id: &str, ticket: Option<u64>) -> bool;
}

/// What a section owes once every guard is released.
#[derive(Debug, Default)]
#[must_use = "run it with InboxHub::after_section once every guard is released"]
pub(crate) struct AfterSection {
    pub(super) wakes: Vec<Recipient>,
}

impl InboxHub {
    /// Run what a section owes: wake the recipients of what it posted and
    /// post the notices it queued. Called with no ranked lock held.
    pub(crate) fn after_section(&self, after: AfterSection) {
        for to in after.wakes {
            match &to.agent_id {
                Some(agent) => self.dispatch_agent(&to.pane_id, agent),
                None => self.dispatch_pane(&to.pane_id),
            }
        }
        self.post_owed_notices();
    }

    /// Install `key`'s publication (D7), before the spawn reservation of any
    /// generation it covers. `Ok(false)` when one is already there, which is
    /// left as it is.
    pub(crate) fn install_publication(
        &self,
        _returns: &mut ReturnsGuard<'_>,
        key: PublishKey,
        publication: Publication,
    ) -> Result<bool, crate::guarded::Torn> {
        let mut st = self.lock();
        st.guarded_section(|_, reports| {
            if reports.publications.contains_key(&key) {
                return false;
            }
            reports.publications.insert(key, publication);
            true
        })
    }

    /// Remove `key`'s initial publication from a destructor (a dispatch that
    /// returned an error or unwound before registering its unit): the hub through
    /// `lock_in_drop(Recover)`; `Err` on a rank violation or torn state, which
    /// changes nothing.
    pub(crate) fn remove_publication_in_drop(
        &self,
        _returns: &mut ReturnsGuard<'_>,
        key: &PublishKey,
    ) -> Result<(), ()> {
        let Some(mut st) = self.lock_in_drop(crate::lock_rank::OnPoison::Recover) else {
            return Err(());
        };
        st.guarded_section(|_, reports| {
            if matches!(reports.publications.get(key), Some(Publication::Initial)) {
                reports.publications.remove(key);
            }
        })
        .map_err(|_| ())
    }

    /// A report or dispatch result whose post was refused, or whose
    /// recipient is gone: a recovery row now, and its file's seven days start.
    pub(crate) fn set_aside(&self, entry: AcceptedReport, cause: OutcomeCause) {
        let now = crate::retained_store::Now::current();
        let mut st = self.lock();
        let store = st.retained.clone();
        let report = entry.report_id.clone();
        let set = st.guarded_section(|_, reports| {
            if report
                .as_deref()
                .is_none_or(|report| !reports.outcomes.has_report(report))
            {
                reports.outcomes.insert(
                    entry.into_row(cause, chrono::Utc::now().timestamp_millis()),
                    None,
                );
            }
            // A result held in memory starts its seven days too.
            if let Some(body) = report
                .as_deref()
                .and_then(|report| reports.fallback.get_mut(report))
            {
                body.lifetime.recover(now);
            }
        });
        if set.is_err() {
            st.log_torn_once("a refused result");
        }
        drop(st);
        if let (Some(store), Some(report)) = (store, report) {
            store.recovered(&report, now);
        }
    }

    /// Hold a dispatch result its file could not take, in memory under its
    /// report id (§3.2). `false` when the report state is torn: the room is
    /// given back and the body is not held.
    pub(crate) fn hold_fallback(
        &self,
        report_id: &str,
        contents: Vec<u8>,
        held: crate::retained_store::HeldResult,
        store: &crate::retained_store::RetainedStore,
    ) -> bool {
        let mut st = self.lock();
        let mut held = Some(held);
        let kept = st.guarded_section(|_, reports| {
            if let Some(held) = held.take() {
                reports.fallback.insert(
                    report_id.to_string(),
                    crate::report_recovery::FallbackBody {
                        contents,
                        held_since: Instant::now(),
                        held: Some(held),
                        lifetime: crate::retained_store::Lifetime::Referenced,
                    },
                );
            }
        });
        if kept.is_err() {
            st.log_torn_once("a held dispatch result");
        }
        drop(st);
        if let Some(held) = held {
            store.end_held(held);
            return false;
        }
        true
    }

    /// Retry saving every held dispatch result, and end those whose lifetime
    /// is over (§3.2). Each body's room is taken out under the lock and the
    /// body written off it; the body itself stays held meanwhile, so an
    /// acknowledgement or a recipient's exit during the write still settles
    /// its lifetime. A saved body's file then takes that lifetime, and its
    /// rows and ledger entries move to `artifact`; the rest get their room
    /// back.
    pub(crate) fn retry_fallback_bodies(&self, store: &crate::retained_store::RetainedStore) {
        self.retry_fallback_bodies_at(store, crate::retained_store::Now::current());
    }

    /// [`Self::retry_fallback_bodies`] at `now`, which a test moves forward.
    pub(crate) fn retry_fallback_bodies_at(
        &self,
        store: &crate::retained_store::RetainedStore,
        now: crate::retained_store::Now,
    ) {
        let _pass = self
            .fallback_retry_pass
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let taken = {
            let mut st = self.lock();
            st.guarded_section(|_, reports| {
                let mut ended = Vec::new();
                let mut writes = Vec::new();
                let ids: Vec<String> = reports.fallback.keys().cloned().collect();
                for id in ids {
                    let Some(body) = reports.fallback.get_mut(&id) else {
                        continue;
                    };
                    if body.lifetime.ends().is_some_and(|at| now.mono >= at) {
                        if let Some(body) = reports.fallback.remove(&id)
                            && let Some(held) = body.held
                        {
                            ended.push(held);
                        }
                    } else if let Some(held) = body.held.take() {
                        writes.push((id, body.contents.clone(), held));
                    }
                }
                (ended, writes)
            })
        };
        let Ok((ended, writes)) = taken else {
            return;
        };
        for held in ended {
            store.end_held(held);
        }
        #[cfg(test)]
        if let Some((reached, release)) = self.hold_fallback_retry.lock().unwrap().take() {
            let _ = reached.send(());
            let _ = release.recv();
        }
        let mut saved = Vec::new();
        let mut unsaved = Vec::new();
        for (report_id, contents, held) in writes {
            match store.retry_held(held, &report_id, &contents, now) {
                Ok(()) => saved.push(report_id),
                Err(held) => unsaved.push((report_id, held)),
            }
        }
        let mut st = self.lock();
        let mut unsaved = Some(unsaved);
        let restored = st.guarded_section(|_, reports| {
            for report_id in &saved {
                // Under the hub's lock, so an event cannot land between the
                // body leaving and its file taking the body's lifetime.
                if let Some(body) = reports.fallback.remove(report_id) {
                    store.continue_lifetime(report_id, body.lifetime);
                }
                reports.outcomes.saved(report_id);
                for entry in reports.accepted.values_mut() {
                    if entry.report_id.as_deref() == Some(report_id.as_str()) {
                        entry.storage = OutcomeStorage::Artifact;
                    }
                }
            }
            let mut stray = Vec::new();
            for (report_id, held) in unsaved.take().into_iter().flatten() {
                match reports.fallback.get_mut(&report_id) {
                    Some(body) => body.held = Some(held),
                    None => stray.push(held),
                }
            }
            stray
        });
        let stray = match restored {
            Ok(stray) => stray,
            Err(_) => {
                st.log_torn_once("a fallback retry");
                // The bodies went with the torn state: their room comes back.
                unsaved
                    .take()
                    .into_iter()
                    .flatten()
                    .map(|(_, held)| held)
                    .collect()
            }
        };
        drop(st);
        for held in stray {
            store.end_held(held);
        }
    }

    /// Retry the held dispatch results soon, at most once a minute, off the
    /// runtime's workers (§3.2: a refused dispatch while results are held).
    pub(crate) fn retry_fallback_soon(self: &Arc<Self>) {
        let Some(store) = self.retained_store() else {
            return;
        };
        {
            let mut last = self
                .fallback_retry
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if last.is_some_and(|at: Instant| at.elapsed() < Duration::from_secs(60)) {
                return;
            }
            *last = Some(Instant::now());
        }
        let hub = Arc::clone(self);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            drop(runtime.spawn_blocking(move || hub.retry_fallback_bodies(&store)));
        }
    }

    /// Whether the report state is torn.
    pub(crate) fn reports_torn(&self) -> bool {
        self.lock().guarded_read(|_| ()).is_err()
    }

    /// A unit's route registration (D7), one R1 → R2 → R3 (→ R4) section with
    /// the R1 read done by the caller: it removes the dispatch's initial
    /// publication, indexes the terminal generation and pins the unit on it,
    /// and either inserts the route or, when the generation already exited or
    /// is closing, records the unit's end and its `dispatch ended` outcome.
    pub(crate) fn unit_route_registered(
        &self,
        returns: &mut ReturnsGuard<'_>,
        view: &dyn RecordView,
        registration: Registration,
    ) -> AfterSection {
        let mut after = AfterSection::default();
        let mut st = self.lock();
        let store = st.retained.clone();
        let route = RetainedReturn {
            unit_agent_id: registration.agent_id.clone(),
            caller: registration.caller.clone(),
            unit_id: registration.unit_id.clone(),
            orphaned: false,
        };
        let section = st.guarded_section(|st, reports| {
            #[cfg(test)]
            if let Some(hook) = seams::take_registration_panic() {
                hook();
            }
            reports.publications.remove(&registration.publication);
            reports.units.index(
                &registration.pane_id,
                &registration.agent_id,
                &registration.unit_id,
            );
            view.pin_unit(&registration.agent_id, &registration.unit_id, None);
            match registration.ended {
                None => {
                    let displaced = returns.register(
                        &registration.pane_id,
                        &registration.agent_id,
                        &registration.unit_id,
                        registration.caller.clone(),
                    );
                    if displaced.is_some() {
                        tracing::debug!(
                            pane_id = %registration.pane_id,
                            "dispatch return: a recycled unit pane displaced an undelivered entry"
                        );
                    }
                }
                Some(how) => {
                    reports
                        .units
                        .set(&registration.unit_id, UnitEntry::Ended { how });
                    dispatch_ended(st, reports, &route, &registration.pane_id, how, &mut after);
                }
            }
            evict_units(reports, view);
            drain_events_in_section(st, reports, &mut StoreAccess::Take(store.as_deref()));
        });
        if section.is_err() {
            st.log_torn_once("unit registration");
            // As before round 5: the route goes in, whatever the unit's state.
            returns.register(
                &registration.pane_id,
                &registration.agent_id,
                &registration.unit_id,
                registration.caller,
            );
        }
        after
    }

    /// Route termination (D7), one R2 → R3 (→ R4) section: for each route the
    /// caller removed under R2, by the unit's entry — a claim is marked
    /// `unit_gone` (its report still reaches the snapshot caller), a unit that
    /// reported or already ended emits nothing, and otherwise the unit's end is
    /// recorded and its caller told once. A close that settles a replacement
    /// in progress keeps that publication, marked closed.
    pub(crate) fn unit_routes_ended(
        &self,
        _returns: &mut ReturnsGuard<'_>,
        view: &dyn RecordView,
        removed: Vec<RetainedReturn>,
        pane_id: &str,
        how: EndedHow,
    ) -> AfterSection {
        let mut after = AfterSection::default();
        if removed.is_empty() && how != EndedHow::Closed {
            return after;
        }
        let mut st = self.lock();
        let store = st.retained.clone();
        let section = st.guarded_section(|st, reports| {
            for route in &removed {
                settle_unit_end(st, reports, route, pane_id, how, &mut after);
            }
            // A close of a pane whose unit's respawn is in progress settles the
            // unit itself, and keeps the publication for the respawn to remove.
            if how == EndedHow::Closed
                && let Some(Publication::Replacement { unit, closed, .. }) = reports
                    .publications
                    .get_mut(&PublishKey::Pane(pane_id.to_string()))
                && !*closed
            {
                *closed = true;
                let unit = unit.clone();
                if reports.units.get(&unit).is_none() {
                    reports.units.set(&unit, UnitEntry::Ended { how });
                    // The caller, from the route the respawn's predecessor
                    // held, when the close found one above.
                    if let Some(route) = removed.iter().find(|route| route.unit_id == unit) {
                        dispatch_ended(st, reports, route, pane_id, how, &mut after);
                    }
                }
            }
            evict_units(reports, view);
            drain_events_in_section(st, reports, &mut StoreAccess::Take(store.as_deref()));
        });
        if section.is_err() {
            st.log_torn_once("unit route termination");
        }
        after
    }

    /// The recovery rows, for `ListOutcomes`. `Err` when the report state is
    /// torn: no listing is ever read from it then.
    pub(crate) fn deck_outcomes(
        &self,
    ) -> Result<crate::report_recovery::DeckOutcomes, crate::guarded::Torn> {
        let st = self.lock();
        let deck = st.inbox.deck().to_string();
        st.guarded_read(|reports| crate::report_recovery::DeckOutcomes {
            deck_id: deck,
            incarnation: reports.incarnation.clone(),
            revision: reports.outcomes.revision(),
            overflow: reports.outcomes.overflow(),
            outcomes: reports.outcomes.rows(),
        })
    }

    /// Test probe: read the report state.
    #[cfg(test)]
    pub(crate) fn read_reports<R>(
        &self,
        f: impl FnOnce(&ReportState) -> R,
    ) -> Result<R, crate::guarded::Torn> {
        self.lock().guarded_read(f)
    }

    /// Test seam: record units that ended, each indexed to one generation,
    /// then evict past the bound, as a section that writes unit entries does.
    #[cfg(test)]
    pub(crate) fn insert_ended_units_for_test(
        &self,
        view: &dyn RecordView,
        units: &[(&str, &str, &str)],
    ) {
        let mut st = self.lock();
        let _ = st.guarded_section(|_, reports| {
            for (pane, agent, unit) in units {
                reports.units.index(pane, agent, unit);
                reports.units.set(
                    unit,
                    UnitEntry::Ended {
                        how: EndedHow::Exited,
                    },
                );
                evict_units(reports, view);
            }
        });
    }

    /// Test seam: run `f` inside a guarded section on this thread, as a
    /// section that writes the report state does; a panic in `f` tears it.
    #[cfg(test)]
    pub(crate) fn run_guarded_for_test(&self, f: impl FnOnce(&mut ReportState)) {
        let mut st = self.lock();
        let _ = st.guarded_section(|_, reports| f(reports));
    }
}

/// How a replacement's transition settled, as the successor's reader is
/// told (D7 "B's reader waits for a completed transition").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplacementOutcome {
    /// The route, index and unit now name the successor.
    Published,
    /// The unit ended (or its claim was marked); the successor is indexed to
    /// it and is being closed.
    Aborted,
    /// The report state was torn or the routes unavailable: nothing was
    /// settled, and nothing in-process will settle it.
    Unavailable,
}

impl InboxHub {
    /// The replacement's `begin` (D7 step 1): if `(pane, from)` is a unit's
    /// terminal, install its replacement publication under `ticket` and answer
    /// the unit. `None` — the respawn then runs as before round 5 — when it is
    /// no unit's terminal, when a publication already covers the pane (a key
    /// the plan did not foresee, logged), or when the report state is torn.
    pub(crate) fn replacement_begin(
        &self,
        _returns: &mut ReturnsGuard<'_>,
        pane_id: &str,
        from: &str,
        ticket: u64,
    ) -> Option<String> {
        let mut st = self.lock();
        let begun = st.guarded_section(|_, reports| {
            let unit = reports.units.unit_of(pane_id, from)?.to_string();
            let key = PublishKey::Pane(pane_id.to_string());
            if reports.publications.contains_key(&key) {
                tracing::error!(
                    pane_id = %pane_id,
                    "respawn: a publication already covers this pane; the replacement runs \
                     without one"
                );
                return None;
            }
            reports.publications.insert(
                key,
                Publication::Replacement {
                    unit: unit.clone(),
                    from: from.to_string(),
                    ticket,
                    closed: false,
                },
            );
            Some(unit)
        });
        match begun {
            Ok(unit) => unit,
            Err(_) => {
                st.log_torn_once("respawn begin");
                None
            }
        }
    }

    /// The replacement's `publish` (D7 step 2), after the leaf moved the unit
    /// to `successor`: compare-and-set the route from `from` to the successor,
    /// index the successor, pin the unit on it (only when it was reserved
    /// under `ticket`) and remove the publication. A publication a close
    /// settled leaves the route alone. Idempotent on the unit's keys.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replacement_publish(
        &self,
        returns: &mut ReturnsGuard<'_>,
        view: &dyn RecordView,
        pane_id: &str,
        unit_id: &str,
        from: &str,
        ticket: u64,
        successor: &str,
    ) -> Result<(), crate::guarded::Torn> {
        let mut st = self.lock();
        let published = st.guarded_section(|_, reports| {
            let key = PublishKey::Pane(pane_id.to_string());
            let closed = match reports.publications.get(&key) {
                Some(Publication::Replacement {
                    ticket: held,
                    closed,
                    ..
                }) if *held == ticket => *closed,
                // Already settled by this replacement: nothing to do.
                _ => return,
            };
            if !closed {
                returns.retarget(pane_id, unit_id, from, successor);
            }
            #[cfg(test)]
            if let Some(hook) = seams::take_publish_panic() {
                hook();
            }
            reports.units.index(pane_id, successor, unit_id);
            view.pin_unit(successor, unit_id, Some(ticket));
            reports.publications.remove(&key);
        });
        if published.is_err() {
            st.log_torn_once("respawn publish");
        }
        published
    }

    /// The replacement's `abort` (D7 step 3), in ordinary code after a spawn
    /// error or a caught panic: unit-keyed and idempotent. Nothing when the
    /// publication is already gone. Otherwise the unit's route is taken
    /// whichever generation it names, the unit settled as a termination
    /// settles it (`RespawnFailed`), the successor — only the one the ticket
    /// reserved, never another occupant of the pane — indexed and pinned, and
    /// the publication removed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replacement_abort(
        &self,
        returns: &mut ReturnsGuard<'_>,
        view: &dyn RecordView,
        pane_id: &str,
        unit_id: &str,
        ticket: u64,
        successor: Option<&str>,
    ) -> Result<AfterSection, crate::guarded::Torn> {
        let mut after = AfterSection::default();
        let mut st = self.lock();
        let store = st.retained.clone();
        let aborted = st.guarded_section(|st, reports| {
            let key = PublishKey::Pane(pane_id.to_string());
            match reports.publications.get(&key) {
                Some(Publication::Replacement { ticket: held, .. }) if *held == ticket => {}
                _ => return,
            }
            #[cfg(test)]
            if let Some(hook) = seams::take_abort_panic() {
                hook();
            }
            if let Some(route) = returns.take_unit(unit_id) {
                settle_unit_end(
                    st,
                    reports,
                    &route,
                    pane_id,
                    EndedHow::RespawnFailed,
                    &mut after,
                );
            } else if reports.units.get(unit_id).is_none() {
                reports.units.set(
                    unit_id,
                    UnitEntry::Ended {
                        how: EndedHow::RespawnFailed,
                    },
                );
            }
            if let Some(successor) = successor {
                reports.units.index(pane_id, successor, unit_id);
                view.pin_unit(successor, unit_id, Some(ticket));
            }
            reports.publications.remove(&key);
            evict_units(reports, view);
            drain_events_in_section(st, reports, &mut StoreAccess::Take(store.as_deref()));
        });
        match aborted {
            Ok(()) => Ok(after),
            Err(torn) => {
                st.log_torn_once("respawn abort");
                Err(torn)
            }
        }
    }

    /// Test probe: the publication on `pane_id`, if any.
    #[cfg(test)]
    pub(crate) fn publication_on(&self, pane_id: &str) -> Option<Publication> {
        self.lock()
            .guarded_read(|reports| {
                reports
                    .publications
                    .get(&PublishKey::Pane(pane_id.to_string()))
                    .cloned()
            })
            .ok()
            .flatten()
    }
}

/// Test seams: code run inside the replacement's sections, process-wide (the
/// respawn runs on a runtime worker, not the test's thread).
#[cfg(test)]
pub(crate) mod seams {
    use std::sync::Mutex;

    type Hook = Box<dyn FnOnce() + Send>;

    static PUBLISH: Mutex<Option<Hook>> = Mutex::new(None);
    static ABORT: Mutex<Option<Hook>> = Mutex::new(None);

    /// Run `hook` inside the next `replacement_publish` section, between the
    /// compare-and-set and the index.
    pub(crate) fn on_next_publish(hook: impl FnOnce() + Send + 'static) {
        *PUBLISH.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    /// Run `hook` inside the next `replacement_abort` section.
    pub(crate) fn on_next_abort(hook: impl FnOnce() + Send + 'static) {
        *ABORT.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    pub(crate) fn take_publish_panic() -> Option<Hook> {
        PUBLISH.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    pub(crate) fn take_abort_panic() -> Option<Hook> {
        ABORT.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    static REGISTRATION: Mutex<Option<Hook>> = Mutex::new(None);

    /// Run `hook` inside the next unit route registration's section.
    pub(crate) fn on_next_registration(hook: impl FnOnce() + Send + 'static) {
        *REGISTRATION.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(hook));
    }

    pub(crate) fn take_registration_panic() -> Option<Hook> {
        REGISTRATION
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

/// What [`InboxHub::unit_route_registered`] registers.
#[derive(Debug, Clone)]
pub(crate) struct Registration {
    pub unit_id: String,
    pub pane_id: String,
    pub agent_id: String,
    pub caller: DispatchCaller,
    /// The dispatch's initial publication, removed in this section.
    pub publication: PublishKey,
    /// The terminal generation already exited, or its pane is closing.
    pub ended: Option<EndedHow>,
}

/// Settle one removed route's unit (D7 "Unit-side termination" step 3).
fn settle_unit_end(
    st: &mut HubState,
    reports: &mut ReportState,
    route: &RetainedReturn,
    pane_id: &str,
    how: EndedHow,
    after: &mut AfterSection,
) {
    match reports.units.get(&route.unit_id) {
        Some(UnitEntry::Claimed { claim_id }) => {
            // The claim commits to its snapshot caller; nothing is emitted now
            // (D12 rule 7). A failure of the claim ends the unit instead.
            let claim_id = *claim_id;
            if let Some(claim) = reports.claims.get_mut(claim_id) {
                claim.unit_gone.get_or_insert(how);
            }
        }
        Some(UnitEntry::Completed { .. } | UnitEntry::Ended { .. }) => {}
        None => {
            reports.units.set(&route.unit_id, UnitEntry::Ended { how });
            dispatch_ended(st, reports, route, pane_id, how, after);
        }
    }
}

/// [`dispatch_ended`] for a failed terminal claim whose unit ended while it
/// was being saved: the claim's snapshot caller is told (D12 rule 9).
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_ended_for(
    st: &mut HubState,
    reports: &mut ReportState,
    unit_id: &str,
    unit_name: &str,
    caller: &Recipient,
    unit_pane_id: &str,
    unit_agent_id: &str,
    how: EndedHow,
    after: &mut AfterSection,
) {
    let route = RetainedReturn {
        unit_agent_id: unit_agent_id.to_string(),
        caller: DispatchCaller {
            pane_id: caller.pane_id.clone(),
            agent_id: caller.agent_id.clone().unwrap_or_default(),
            unit_name: unit_name.to_string(),
        },
        unit_id: unit_id.to_string(),
        orphaned: false,
    };
    dispatch_ended(st, reports, &route, unit_pane_id, how, after);
}

/// A unit's `dispatch ended` outcome (D7): a deck notice in a live caller's
/// inbox, held in the accepted-report ledger like any dispatch result, or a
/// recovery row when the caller is orphaned, gone, or its inbox refused it.
fn dispatch_ended(
    st: &mut HubState,
    reports: &mut ReportState,
    route: &RetainedReturn,
    unit_pane_id: &str,
    how: EndedHow,
    after: &mut AfterSection,
) {
    let name = route.caller.unit_name.clone();
    let text = crate::report_recovery::cut_bytes(
        &format!(
            "Dispatched unit {} ended ({}) without reporting.",
            crate::state::quote_untrusted_role(&name),
            how.clause()
        ),
        crate::report_recovery::MAX_SUMMARY_BYTES,
    );
    let caller = Recipient::bound(&route.caller.pane_id, &route.caller.agent_id);
    let entry = AcceptedReport {
        provenance: ReportProvenance::DispatchResult {
            unit_id: Some(route.unit_id.clone()),
            phase: DispatchPhase::Ended,
            report_id: None,
        },
        phase: OutcomePhase::DispatchEnded,
        report_id: None,
        summary: text.clone(),
        storage: OutcomeStorage::None,
        label: crate::report_recovery::wire_text(
            &format!("dispatch ended: {name}"),
            crate::report_recovery::MAX_LABEL_CHARS,
        ),
        original: crate::report_recovery::party(&caller, None),
        producer: OutcomeParty {
            pane_id: unit_pane_id.to_string(),
            agent_id: route.unit_agent_id.clone(),
            role: None,
        },
        unit: Some(OutcomeUnit {
            unit_id: route.unit_id.clone(),
            name: crate::report_recovery::wire_text(&name, crate::report_recovery::MAX_LABEL_CHARS),
        }),
        orchestration: None,
        task_id: None,
        handed: false,
    };
    let live = !route.orphaned
        && st
            .live_agent
            .as_ref()
            .is_none_or(|check| check(&route.caller.pane_id, &route.caller.agent_id));
    let now_ms = chrono::Utc::now().timestamp_millis();
    if !live {
        reports
            .outcomes
            .insert(entry.into_row(OutcomeCause::RecipientExited, now_ms), None);
        return;
    }
    let mut content = Content::from_deck(
        crate::dispatch_return::dispatch_reason(&name),
        "dispatch ended",
        text,
    );
    content.provenance = Some(entry.provenance.clone());
    match st
        .inbox
        .post_direct(Instant::now(), caller.clone(), content)
    {
        Ok(id) => {
            reports.accepted.insert(id.to_string(), entry);
            after.wakes.push(caller);
        }
        Err(error) => {
            tracing::warn!(
                pane_id = %route.caller.pane_id,
                %error,
                "dispatch: the caller's inbox refused the dispatch-ended notice; it is kept as a \
                 recovery row"
            );
            reports
                .outcomes
                .insert(entry.into_row(OutcomeCause::RecipientRefused, now_ms), None);
        }
    }
}

/// Evict units past the bound, never one a publication names or one a
/// still-registered generation can attest as (D2).
fn evict_units(reports: &mut ReportState, view: &dyn RecordView) {
    let publications = &reports.publications;
    reports.units.evict(|unit, generations| {
        publications.values().any(|p| p.names_unit(unit))
            || generations.iter().any(|(_, agent)| view.registered(agent))
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use spec::spec;

    use super::*;
    use crate::report_recovery::{OutcomeParty, ReportProvenance};

    /// A small deterministic generator, so a failing sequence replays.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn entry(report: &str, pane: &str, agent: &str) -> AcceptedReport {
        let party = OutcomeParty {
            pane_id: pane.to_string(),
            agent_id: agent.to_string(),
            role: None,
        };
        AcceptedReport {
            provenance: ReportProvenance::Uncorrelated {
                report_id: report.to_string(),
                producer: crate::report_recovery::Producer {
                    pane_id: "worker".to_string(),
                    agent_id: "w".to_string(),
                    role: None,
                },
            },
            phase: OutcomePhase::WorkerReport,
            report_id: Some(report.to_string()),
            summary: String::new(),
            storage: OutcomeStorage::Artifact,
            label: "report".to_string(),
            original: party.clone(),
            producer: party,
            unit: None,
            orchestration: None,
            task_id: None,
            handed: false,
        }
    }

    /// Scenario: Random sequences of posted reports, acknowledgements and recipient exits, over three agents and twelve seeds, on a real inbox hub. After each sequence every posted report is in exactly one place: acknowledged (no ledger entry, no row), set aside (a row naming its report, no ledger entry), or still live (a ledger entry, no row).
    #[spec("inbox/recovery/013")]
    #[test]
    fn recovery_013_every_accepted_report_ends_in_exactly_one_place() {
        for seed in 1..=12u64 {
            let hub = Arc::new(InboxHub::new(
                DeckId::generate(),
                crate::inbox::InboxLimits::default(),
                Duration::from_secs(30),
            ));
            let agents = [("p0", "a0"), ("p1", "a1"), ("p2", "a2")];
            let alive: Arc<std::sync::Mutex<[bool; 3]>> =
                Arc::new(std::sync::Mutex::new([true; 3]));
            let live = Arc::clone(&alive);
            hub.set_live_agent_check(Arc::new(move |pane: &str, _: &str| {
                let n = pane[1..].parse::<usize>().unwrap();
                live.lock().unwrap()[n]
            }));
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            // report id -> (message id, recipient index, acknowledged)
            let mut posted: HashMap<String, (String, usize, bool)> = HashMap::new();
            for step in 0..60 {
                let n = rng.below(3) as usize;
                let (pane, agent) = agents[n];
                match rng.below(4) {
                    0 | 1 => {
                        let report = format!("r{seed}-{step}");
                        let Ok(room) = hub.reserve_reply() else {
                            continue;
                        };
                        let to = Recipient::bound(pane, agent);
                        let is_live = alive.lock().unwrap()[n];
                        let mut content = Content::from_deck("notice", "report", "body");
                        content.provenance = Some(entry(&report, pane, agent).provenance);
                        if let Ok(id) = hub.post_reserved_reply_where_recorded(
                            room,
                            to,
                            content,
                            &|| is_live,
                            Some(entry(&report, pane, agent)),
                        ) {
                            posted.insert(report, (id.to_string(), n, false));
                        }
                    }
                    2 => {
                        let pick: Vec<String> = posted
                            .iter()
                            .filter(|(_, (_, at, acked))| *at == n && !acked)
                            .map(|(report, _)| report.clone())
                            .collect();
                        if let Some(report) = pick.first() {
                            let id = posted[report].0.clone();
                            if hub.ack_in_store_for_test(&id, pane, agent)
                                == crate::inbox::AckOutcome::Acknowledged
                            {
                                posted.get_mut(report).unwrap().2 = true;
                            }
                        }
                    }
                    _ => {
                        if alive.lock().unwrap()[n] {
                            alive.lock().unwrap()[n] = false;
                            let _ = hub.agent_exited(pane, agent);
                        }
                    }
                }
            }
            let (ledger, rows) = hub
                .read_reports(|r| {
                    (
                        r.accepted
                            .values()
                            .filter_map(|e| e.report_id.clone())
                            .collect::<Vec<_>>(),
                        r.outcomes
                            .rows()
                            .into_iter()
                            .filter_map(|row| row.report_id)
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap();
            for (report, (_, _, acked)) in &posted {
                let in_ledger = ledger.iter().filter(|r| *r == report).count();
                let in_rows = rows.iter().filter(|r| *r == report).count();
                let places = in_ledger + in_rows + usize::from(*acked);
                assert_eq!(
                    places, 1,
                    "seed {seed}: report {report} is in {places} places (ledger {in_ledger}, \
                     rows {in_rows}, acknowledged {acked})"
                );
            }
        }
    }
}
