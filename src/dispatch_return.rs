//! PRD #220 Phase 2: the dispatch RETURN edge — who a dispatched unit reports
//! back to when it finishes, and what that report says.
//!
//! `dispatch` already delivers an ACKNOWLEDGEMENT into the caller's pane, bound
//! to the caller's registry agent id so it cannot land on whoever inherited a
//! recycled pane id (issue #617 finding 3). That identity was used once,
//! synchronously, and then discarded — so when the unit it started finished
//! there was no address left to deliver to, and a dispatcher could start work
//! but never hear about it again.
//!
//! This module is the retention half. It is deliberately pure data: no locks, no
//! I/O and no registry access, so the eviction rules can be asserted directly
//! rather than through PTYs. [`crate::agent_pty::AgentPtyRegistry`] owns one
//! behind its own mutex, and [`crate::state::AppState::handle_work_done`]
//! resolves through it.

use std::collections::HashMap;

/// The pane that ran `dispatch`, retained until the unit it started completes.
///
/// The pair is captured from ONE `AgentRecord` at dispatch time and kept
/// together for the same reason it is read together (issue #617 finding 3): the
/// report must reach the agent that ASKED, not whoever holds its pane id when
/// the work finishes. A `(name, cwd)` tuple lookup — the route the ordinary
/// worker→orchestrator feedback takes — can never resolve this one, because the
/// caller lives in a different cwd from the unit by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchCaller {
    /// The caller's `DOT_AGENT_DECK_PANE_ID`.
    pub pane_id: String,
    /// The registry agent id occupying that pane when the dispatch was
    /// requested. The identity gate on delivery compares against this, so a
    /// pane that changed hands in between is refused rather than written to.
    pub agent_id: String,
    /// The name the caller passed to `dispatch <name>`, quoted back verbatim in
    /// the completion report so the caller can tell several units apart.
    pub unit_name: String,
}

/// Dispatched units that still owe their caller a completion report, keyed by
/// the unit's TERMINAL pane — `SpawnHandle::delivery_pane_id`, i.e. the single
/// agent's pane or the orchestration's start-role pane.
///
/// Keyed by that pane and not by the worktree because it is exactly the pane id
/// a terminal `work-done --done` arrives under, so resolution is a map lookup
/// rather than a cwd reconstruction. It also means an ordinary worker's
/// completion inside a dispatched orchestration can never match: the workers'
/// panes are not keys.
///
/// The map is bounded by eviction on all three ways an entry can stop being
/// deliverable — the report is delivered ([`Self::take`]), the caller's pane
/// goes away, or the unit's own pane closes ([`Self::evict_pane`] covers the
/// last two).
#[derive(Debug, Default)]
pub struct DispatchReturns {
    by_unit_pane: HashMap<String, DispatchCaller>,
}

impl DispatchReturns {
    /// Retain `caller` as the recipient for the unit whose terminal pane is
    /// `unit_pane_id`. Returns the entry it displaced, if any — a pane id is a
    /// recycled handle, so a stale entry for a pane being re-used is replaced
    /// rather than kept beside the live one.
    pub fn register(
        &mut self,
        unit_pane_id: &str,
        caller: DispatchCaller,
    ) -> Option<DispatchCaller> {
        self.by_unit_pane.insert(unit_pane_id.to_string(), caller)
    }

    /// Who the unit at `unit_pane_id` owes a report to, without consuming it.
    pub fn resolve(&self, unit_pane_id: &str) -> Option<&DispatchCaller> {
        self.by_unit_pane.get(unit_pane_id)
    }

    /// Resolve AND evict — the delivery path. The eviction is unconditional on
    /// the delivery's outcome: a refused report is terminal and never retried
    /// (a retry could only re-target whoever now occupies the caller's pane),
    /// which is the same policy the acknowledgement already runs under.
    pub fn take(&mut self, unit_pane_id: &str) -> Option<DispatchCaller> {
        self.by_unit_pane.remove(unit_pane_id)
    }

    /// Drop every entry `pane_id` takes part in — as the dispatched unit's own
    /// pane (its tab closed, so nothing will ever complete) or as the caller's
    /// pane (the recipient is gone, so nothing could be delivered). Returns how
    /// many went, for the caller's log line.
    pub fn evict_pane(&mut self, pane_id: &str) -> usize {
        let before = self.by_unit_pane.len();
        self.by_unit_pane
            .retain(|unit_pane, caller| unit_pane != pane_id && caller.pane_id != pane_id);
        before - self.by_unit_pane.len()
    }

    /// How many units still owe a report. Observability and tests.
    pub fn len(&self) -> usize {
        self.by_unit_pane.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_unit_pane.is_empty()
    }
}

/// The sentence a completed unit's report arrives as, written into the caller's
/// pane as a submitted turn.
///
/// Deliberately mirrors the acknowledgement's shape (`dispatch: spawned isolated
/// agent for '<name>' in <path>`): same `dispatch:` prefix, same single-quoted
/// unit name, so a caller holding several units reads one vocabulary. The report
/// is appended VERBATIM — a multi-line summary is wrapped in bracketed paste by
/// the pane encoder and arrives as one turn, so flattening it here would lose
/// structure the recipient can use and buy nothing.
pub fn compose_completion_report(unit_name: &str, report: &str) -> String {
    let report = report.trim();
    if report.is_empty() {
        return format!("dispatch: unit '{unit_name}' completed. Report: (none given)");
    }
    format!("dispatch: unit '{unit_name}' completed. Report: {report}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(pane: &str, agent: &str, unit: &str) -> DispatchCaller {
        DispatchCaller {
            pane_id: pane.to_string(),
            agent_id: agent.to_string(),
            unit_name: unit.to_string(),
        }
    }

    #[test]
    fn registering_a_dispatch_makes_its_caller_resolvable_by_the_units_pane() {
        let mut returns = DispatchReturns::default();
        assert!(returns.is_empty());
        assert_eq!(
            returns.register("unit-pane", caller("caller-pane", "agent-7", "probe")),
            None,
            "the first registration displaces nothing"
        );
        assert_eq!(
            returns.resolve("unit-pane"),
            Some(&caller("caller-pane", "agent-7", "probe")),
            "the unit's terminal pane must resolve to the pane that dispatched it"
        );
        assert_eq!(returns.len(), 1);
        assert_eq!(
            returns.resolve("some-other-pane"),
            None,
            "a pane nobody dispatched owes nobody a report"
        );
    }

    #[test]
    fn delivering_the_report_evicts_the_entry_so_it_cannot_be_delivered_twice() {
        let mut returns = DispatchReturns::default();
        returns.register("unit-pane", caller("caller-pane", "agent-7", "probe"));

        assert_eq!(
            returns.take("unit-pane"),
            Some(caller("caller-pane", "agent-7", "probe"))
        );
        assert_eq!(
            returns.take("unit-pane"),
            None,
            "a second terminal work-done from the same pane must resolve to nothing; \
             the report is delivered once and the refusal policy is terminal"
        );
        assert!(returns.is_empty());
    }

    #[test]
    fn the_units_own_pane_closing_evicts_the_entry() {
        let mut returns = DispatchReturns::default();
        returns.register("unit-pane", caller("caller-pane", "agent-7", "probe"));
        returns.register("other-unit", caller("caller-pane", "agent-7", "second"));

        assert_eq!(returns.evict_pane("unit-pane"), 1);
        assert_eq!(
            returns.resolve("unit-pane"),
            None,
            "a closed unit tab will never complete, so nothing may keep waiting on it"
        );
        assert!(
            returns.resolve("other-unit").is_some(),
            "closing one unit must not cancel the caller's other outstanding unit"
        );
    }

    #[test]
    fn the_callers_pane_going_away_evicts_every_unit_it_dispatched() {
        let mut returns = DispatchReturns::default();
        returns.register("unit-a", caller("caller-pane", "agent-7", "a"));
        returns.register("unit-b", caller("caller-pane", "agent-7", "b"));
        returns.register("unit-c", caller("other-caller", "agent-9", "c"));

        assert_eq!(
            returns.evict_pane("caller-pane"),
            2,
            "both units dispatched from the closing pane lose their recipient"
        );
        assert!(returns.resolve("unit-a").is_none());
        assert!(returns.resolve("unit-b").is_none());
        assert!(
            returns.resolve("unit-c").is_some(),
            "another caller's outstanding unit is untouched"
        );
        assert_eq!(
            returns.evict_pane("a-pane-in-no-entry"),
            0,
            "an unrelated pane close evicts nothing"
        );
    }

    #[test]
    fn re_registering_a_recycled_unit_pane_replaces_the_stale_caller() {
        let mut returns = DispatchReturns::default();
        returns.register("unit-pane", caller("caller-a", "agent-1", "first"));

        let displaced = returns.register("unit-pane", caller("caller-b", "agent-2", "second"));
        assert_eq!(
            displaced,
            Some(caller("caller-a", "agent-1", "first")),
            "the displaced entry is reported so the daemon can log the recycle"
        );
        assert_eq!(returns.len(), 1);
        assert_eq!(
            returns.resolve("unit-pane").map(|c| c.pane_id.as_str()),
            Some("caller-b"),
            "a pane id is a recycled handle; the newest dispatch owns it"
        );
    }

    #[test]
    fn the_completion_report_names_the_unit_and_carries_its_own_words() {
        let msg = compose_completion_report("verify-pr", "Everything green; PR #12 is mergeable.");
        assert!(
            msg.starts_with("dispatch: "),
            "the return must share the acknowledgement's prefix so a caller reads one \
             vocabulary for both halves of a dispatch: {msg}"
        );
        assert!(
            msg.contains("'verify-pr'"),
            "the unit name must be quoted exactly as `dispatch <name>` took it: {msg}"
        );
        assert!(
            msg.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word == "completed"),
            "the report must say the unit completed, as a whole word: {msg}"
        );
        assert!(
            msg.contains("Everything green; PR #12 is mergeable."),
            "the unit's own report must ride verbatim: {msg}"
        );
        assert!(
            !msg.contains('\n'),
            "a single-line report must stay one line, so it arrives as one turn: {msg}"
        );
    }

    #[test]
    fn a_unit_that_reported_nothing_still_says_so() {
        let msg = compose_completion_report("quiet", "   \n ");
        assert!(
            msg.contains("'quiet'") && msg.contains("completed"),
            "the completion itself is news even with no summary: {msg}"
        );
        assert!(
            msg.contains("(none given)"),
            "an empty report must read as absent rather than as a truncated one: {msg}"
        );
    }
}
