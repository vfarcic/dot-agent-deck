//! Spacing between the starts of orchestration workers' agents (issue #2110).
//!
//! A `clear = true` worker gets a fresh agent for every task, and that agent
//! takes its first turn as soon as the task reaches it. An orchestrator that
//! hands out several tasks at once (a tester, a reviewer and an auditor on one
//! change) therefore starts several agents within the same second, and their
//! first requests reach the provider together. Telemetry round 1 (#1980)
//! recorded one such burst: three workers started inside one second, five
//! sessions were refused with a 429 rate limit seven seconds later, and all
//! three runs ended `failed`.
//!
//! [`RoleStartPacer`] spaces those starts: each one is given the next free slot
//! at least [`role_start_spacing`] after the previous one, and waits for it. A
//! start with no other start in the last spacing window waits for nothing, so a
//! lone delegate is not delayed. The pacer is one per daemon
//! ([`crate::agent_pty::AgentPtyRegistry::role_start_pacer`]) rather than one
//! per orchestration, because the limit the burst hit is the provider
//! account's, which every orchestration on the daemon shares.
//!
//! It paces the `clear = true` restart of the delegate path
//! (`crate::state::dispatch_one_owned`) and nothing else. That path has already
//! answered the orchestrator's `delegate` when it runs, so the wait delays the
//! worker and never a command's reply. `pane restart` answers its caller only
//! after the respawn and starts no turn, and an orchestration's first start
//! hands its workers no task, so neither is paced.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Environment variable setting the spacing, in milliseconds; `0` turns the
/// pacing off. Read at use time, parsed by
/// [`crate::state::parse_bounded_ms_override`], and capped at
/// [`MAX_ROLE_START_SPACING`].
pub const DOT_AGENT_DECK_ROLE_START_SPACING_MS: &str = "DOT_AGENT_DECK_ROLE_START_SPACING_MS";

/// The spacing when [`DOT_AGENT_DECK_ROLE_START_SPACING_MS`] is unset: the
/// issue's "a few seconds". Three workers handed tasks together start 0, 3 and
/// 6 seconds after the delegate, which is short against a task's length and
/// spreads their first requests over more than a provider's per-second burst.
pub const DEFAULT_ROLE_START_SPACING: Duration = Duration::from_secs(3);

/// Ceiling for [`DOT_AGENT_DECK_ROLE_START_SPACING_MS`]. Past a minute the
/// spacing would hold back a worker longer than most providers' rate windows,
/// which is no longer spreading a burst but rationing the account.
pub const MAX_ROLE_START_SPACING: Duration = Duration::from_secs(60);

/// The resolved spacing: [`DOT_AGENT_DECK_ROLE_START_SPACING_MS`] when it
/// parses, [`DEFAULT_ROLE_START_SPACING`] otherwise (a value that does not
/// parse is ignored with a `warn!`, one above the ceiling is clamped).
pub fn role_start_spacing() -> Duration {
    std::env::var(DOT_AGENT_DECK_ROLE_START_SPACING_MS)
        .ok()
        .and_then(|raw| {
            crate::state::parse_bounded_ms_override(
                DOT_AGENT_DECK_ROLE_START_SPACING_MS,
                &raw,
                MAX_ROLE_START_SPACING,
            )
        })
        .unwrap_or(DEFAULT_ROLE_START_SPACING)
}

/// Hands out start slots at least a spacing apart. See the module doc.
#[derive(Debug, Default)]
pub struct RoleStartPacer {
    /// The earliest instant the next start may take, or `None` before the
    /// first start. A leaf lock: nothing else is locked while it is held.
    next_free: Mutex<Option<Instant>>,
}

impl RoleStartPacer {
    /// Reserve the start slot for a start asked for at `now`: `now` itself
    /// when the previous slot is at least `spacing` behind it, otherwise the
    /// instant `spacing` after the previous slot. The reservation is taken
    /// whether or not the caller goes on to start, so a caller that gives up
    /// costs the next one at most one spacing. A zero `spacing` reserves
    /// nothing and answers `now`.
    pub fn reserve(&self, now: Instant, spacing: Duration) -> Instant {
        if spacing.is_zero() {
            return now;
        }
        let mut next_free = self
            .next_free
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = match *next_free {
            Some(at) if at > now => at,
            _ => now,
        };
        *next_free = Some(slot.checked_add(spacing).unwrap_or(slot));
        slot
    }

    /// Reserve a slot with the configured [`role_start_spacing`] and sleep
    /// until it comes. Answers how long it waited (zero when it did not).
    pub async fn wait_for_slot(&self) -> Duration {
        let now = Instant::now();
        let wait = self
            .reserve(now, role_start_spacing())
            .saturating_duration_since(now);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACING: Duration = Duration::from_secs(3);

    /// Scenario: three workers' starts are asked for in the same instant. The
    /// first starts at once and the other two are given slots three and six
    /// seconds later.
    #[test]
    fn role_start_pacing_001_simultaneous_starts_are_spaced_apart() {
        let pacer = RoleStartPacer::default();
        let now = Instant::now();
        let slots: Vec<Duration> = (0..3).map(|_| pacer.reserve(now, SPACING) - now).collect();
        assert_eq!(slots, vec![Duration::ZERO, SPACING, SPACING * 2]);
    }

    /// Scenario: a start is asked for long after the previous one. It starts
    /// at once: the spacing delays only a start that follows another closely.
    #[test]
    fn role_start_pacing_002_a_lone_start_waits_for_nothing() {
        let pacer = RoleStartPacer::default();
        let first = Instant::now();
        assert_eq!(pacer.reserve(first, SPACING), first);
        let later = first + SPACING * 4;
        assert_eq!(pacer.reserve(later, SPACING), later);
    }

    /// Scenario: a second start is asked for one second after the first. It
    /// waits only the remaining two seconds of the spacing, not a whole one.
    #[test]
    fn role_start_pacing_003_a_close_start_waits_only_the_rest_of_the_spacing() {
        let pacer = RoleStartPacer::default();
        let first = Instant::now();
        pacer.reserve(first, SPACING);
        let second = first + Duration::from_secs(1);
        assert_eq!(
            pacer.reserve(second, SPACING) - second,
            Duration::from_secs(2)
        );
    }

    /// Scenario: the spacing is set to zero. Every start is given the instant
    /// it asked at, and a later non-zero spacing counts from no reservation.
    #[test]
    fn role_start_pacing_004_zero_spacing_turns_pacing_off() {
        let pacer = RoleStartPacer::default();
        let now = Instant::now();
        for _ in 0..3 {
            assert_eq!(pacer.reserve(now, Duration::ZERO), now);
        }
        assert_eq!(pacer.reserve(now, SPACING), now);
    }
}
