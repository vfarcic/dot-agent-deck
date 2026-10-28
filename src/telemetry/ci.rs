//! CI time per pull request, for `agent.ci` spans and `deck.ci.change`
//! events: what a run's pull request's checks did, as the deck's own poll of
//! them reported it ([`crate::pull_request_info::PullRequestChecks`]).
//!
//! Pure and synchronous, like the tracker that owns one per pull request,
//! shared by every run whose card holds it, so a pull request's CI history is
//! recorded once however many runs watch it. A stretch opens when the checks
//! are first seen pending and closes when they are seen passing or failing —
//! so its start and end are each as late as one poll of the pull request
//! ([`crate::pull_request::DEFAULT_REFRESH`] unless the user set another
//! interval). The rollup does not say which CI system ran a check, so nothing
//! here does either.

use crate::pull_request_info::{PullRequestChecks, PullRequestChecksState};

use super::span::SpanId;

/// A pull request's overall CI result, as the deck names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    Pending,
    Passing,
    Failing,
}

impl CiState {
    /// `None` for a result this build does not know.
    pub fn from_checks(state: PullRequestChecksState) -> Option<Self> {
        match state {
            PullRequestChecksState::Pending => Some(CiState::Pending),
            PullRequestChecksState::Passing => Some(CiState::Passing),
            PullRequestChecksState::Failing => Some(CiState::Failing),
            PullRequestChecksState::Unknown => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CiState::Pending => "pending",
            CiState::Passing => "passing",
            CiState::Failing => "failing",
        }
    }
}

/// A stretch of pending checks still open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenStretch {
    pub span_id: SpanId,
    pub start: u64,
    pub pr: u64,
}

/// What one observation changed, for the tracker to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiEffect {
    /// The checks' overall result changed: a `deck.ci.change` event.
    Change {
        from: Option<CiState>,
        to: CiState,
        pr: u64,
    },
    /// A stretch ended: an `agent.ci` span. `result` is `None` when it ended
    /// without one (the pull request changed or went away).
    Ended {
        stretch: OpenStretch,
        result: Option<CiState>,
        checks: Option<PullRequestChecks>,
    },
}

/// One pull request's checks, as the runs holding it see them.
#[derive(Debug, Default)]
pub struct PrCi {
    pr: Option<u64>,
    last: Option<CiState>,
    last_checks: Option<PullRequestChecks>,
    open: Option<OpenStretch>,
    /// Stretches that ended in a result.
    pub rounds: u64,
    /// Those that ended failing.
    pub failures: u64,
}

impl PrCi {
    /// A run's card now holds pull request `pr` with `checks`, at `now`.
    /// `new_span_id` is called for a stretch that opens.
    pub fn observe(
        &mut self,
        pr: Option<u64>,
        checks: Option<PullRequestChecks>,
        now: u64,
        new_span_id: impl FnOnce() -> SpanId,
    ) -> Vec<CiEffect> {
        let mut effects = Vec::new();
        if pr != self.pr {
            effects.extend(self.end_open(None));
            self.pr = pr;
            self.last = None;
            self.last_checks = None;
        }
        let Some(pr) = pr else {
            return effects;
        };
        let Some(checks) = checks else {
            return effects;
        };
        let Some(to) = CiState::from_checks(checks.state) else {
            return effects;
        };
        self.last_checks = Some(checks);
        if self.last == Some(to) {
            return effects;
        }
        effects.push(CiEffect::Change {
            from: self.last,
            to,
            pr,
        });
        self.last = Some(to);
        match to {
            CiState::Pending => {
                if self.open.is_none() {
                    self.open = Some(OpenStretch {
                        span_id: new_span_id(),
                        start: now,
                        pr,
                    });
                }
            }
            CiState::Passing | CiState::Failing => {
                if let Some(stretch) = self.open.take() {
                    self.rounds += 1;
                    if to == CiState::Failing {
                        self.failures += 1;
                    }
                    effects.push(CiEffect::Ended {
                        stretch,
                        result: Some(to),
                        checks: Some(checks),
                    });
                }
            }
        }
        effects
    }

    /// The last run holding the pull request let go of it: its open stretch,
    /// if any, ends without a result.
    pub fn end_run(&mut self) -> Option<CiEffect> {
        self.end_open(None)
    }

    /// Whether a stretch of pending checks is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    fn end_open(&mut self, result: Option<CiState>) -> Option<CiEffect> {
        let stretch = self.open.take()?;
        Some(CiEffect::Ended {
            stretch,
            result,
            checks: self.last_checks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(state: PullRequestChecksState, passing: u32, failing: u32) -> PullRequestChecks {
        PullRequestChecks {
            state,
            passing,
            failing,
            pending: 0,
            skipped: 1,
        }
    }

    fn id(n: u8) -> impl FnOnce() -> SpanId {
        move || [n; 8]
    }

    use PullRequestChecksState::{Failing, Passing, Pending};

    #[test]
    fn a_pending_stretch_ends_with_its_result() {
        let mut ci = PrCi::default();
        assert_eq!(
            ci.observe(Some(7), Some(checks(Pending, 0, 0)), 10, id(1)),
            vec![CiEffect::Change {
                from: None,
                to: CiState::Pending,
                pr: 7
            }]
        );
        assert!(
            ci.observe(Some(7), Some(checks(Pending, 1, 0)), 20, id(2))
                .is_empty(),
            "no change, no event"
        );
        let effects = ci.observe(Some(7), Some(checks(Failing, 3, 1)), 30, id(3));
        assert_eq!(
            effects,
            vec![
                CiEffect::Change {
                    from: Some(CiState::Pending),
                    to: CiState::Failing,
                    pr: 7
                },
                CiEffect::Ended {
                    stretch: OpenStretch {
                        span_id: [1; 8],
                        start: 10,
                        pr: 7
                    },
                    result: Some(CiState::Failing),
                    checks: Some(checks(Failing, 3, 1)),
                }
            ]
        );
        ci.observe(Some(7), Some(checks(Pending, 3, 0)), 40, id(4));
        ci.observe(Some(7), Some(checks(Passing, 4, 0)), 50, id(5));
        assert_eq!((ci.rounds, ci.failures), (2, 1));
    }

    #[test]
    fn a_result_without_a_pending_stretch_is_a_change_only() {
        let mut ci = PrCi::default();
        let effects = ci.observe(Some(7), Some(checks(Passing, 2, 0)), 10, id(1));
        assert_eq!(effects.len(), 1);
        assert_eq!(ci.rounds, 0);
        assert!(ci.end_run().is_none());
    }

    #[test]
    fn a_new_or_missing_pull_request_ends_the_stretch_without_a_result() {
        let mut ci = PrCi::default();
        ci.observe(Some(7), Some(checks(Pending, 0, 0)), 10, id(1));
        let effects = ci.observe(Some(8), Some(checks(Pending, 0, 0)), 20, id(2));
        assert!(matches!(
            &effects[0],
            CiEffect::Ended { stretch, result: None, .. } if stretch.pr == 7
        ));
        assert_eq!(
            effects[1],
            CiEffect::Change {
                from: None,
                to: CiState::Pending,
                pr: 8
            },
            "the new pull request starts from nothing"
        );
        let effects = ci.observe(None, None, 30, id(3));
        assert!(matches!(
            &effects[..],
            [CiEffect::Ended { stretch, result: None, .. }] if stretch.pr == 8
        ));
        assert_eq!(ci.rounds, 0);
    }

    #[test]
    fn the_run_ending_ends_the_stretch_with_the_last_counts() {
        let mut ci = PrCi::default();
        ci.observe(Some(7), Some(checks(Pending, 2, 0)), 10, id(1));
        assert_eq!(
            ci.end_run(),
            Some(CiEffect::Ended {
                stretch: OpenStretch {
                    span_id: [1; 8],
                    start: 10,
                    pr: 7
                },
                result: None,
                checks: Some(checks(Pending, 2, 0)),
            })
        );
        assert!(ci.end_run().is_none());
    }

    #[test]
    fn missing_or_unknown_checks_change_nothing() {
        let mut ci = PrCi::default();
        assert!(ci.observe(Some(7), None, 10, id(1)).is_empty());
        assert!(
            ci.observe(
                Some(7),
                Some(checks(PullRequestChecksState::Unknown, 0, 0)),
                20,
                id(2)
            )
            .is_empty()
        );
    }
}
