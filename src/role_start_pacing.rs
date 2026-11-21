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
//! [`RoleStartPacer`] spaces those starts by the time each fresh agent is
//! actually spawned, not by when its restart was asked for. Paced restarts take
//! turns ([`RoleStartPacer::acquire`]): each waits until [`role_start_spacing`]
//! has passed since the previous paced restart finished spawning its agent,
//! then runs its own restart, whose teardown of the old agent only adds to that
//! gap, and records when its agent started ([`PacedStart::started`]). So two
//! fresh agents are never spawned less than the spacing apart, however long
//! each old agent takes to stop (PR #2459 review: spacing the requests alone
//! let a slow teardown and a fast one spawn together). A restart with no other
//! paced start in the last spacing waits for nothing, so a lone delegate is not
//! delayed. The pacer is one per daemon
//! ([`crate::agent_pty::AgentPtyRegistry::role_start_pacer`]) rather than one
//! per orchestration, because the limit the burst hit is the provider
//! account's, which every orchestration on the daemon shares.
//!
//! It paces the `clear = true` restart of the delegate path
//! (`crate::state::dispatch_one_owned`) and nothing else. That path runs
//! detached from the `delegate` handler, which waits for the tasks to be posted
//! for at most `DELEGATE_POST_WAIT` and then replies anyway, so a paced restart
//! can hold a `delegate` reply for that bounded window and no longer.
//! `pane restart` answers its caller only after the respawn, within a fixed
//! budget, and starts no turn; an orchestration's first start hands its workers
//! no task. Neither is paced.
//!
//! The wait happens while the old agent still runs, before the respawn lifts
//! its record, so the pane's remove→replace window stays as short as it was
//! (closes and `restart-daemon` bound that window by
//! [`crate::agent_pty::RESPAWN_SETTLE_TIMEOUT`]). A turn is held through the
//! whole restart, so a restart stuck in its own recovery holds the next paced
//! restart behind it for as long; that recovery is itself bounded
//! (`PANE_CLOSE_RECREATE_TIMEOUT`).

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

/// Takes paced restarts one at a time, a spacing apart. See the module doc.
#[derive(Debug, Default)]
pub struct RoleStartPacer {
    /// When the last paced restart finished spawning its agent, or `None`
    /// before the first. Held by a [`PacedStart`] for its whole restart, which
    /// is what makes paced restarts take turns.
    last_start: tokio::sync::Mutex<Option<Instant>>,
}

/// How long a restart asked for at `now` must wait, when the previous paced
/// agent started at `last_start`: whatever is left of `spacing` since then.
pub fn wait_before_start(last_start: Option<Instant>, now: Instant, spacing: Duration) -> Duration {
    last_start
        .and_then(|last| last.checked_add(spacing))
        .map_or(Duration::ZERO, |free| free.saturating_duration_since(now))
}

impl RoleStartPacer {
    /// Take this restart's turn: wait for every earlier paced restart to
    /// finish, then work out how long this one must still wait
    /// ([`PacedStart::wait`]). `on_delay` runs once, before any waiting, when
    /// the restart will be held back at all, whether behind another restart's
    /// turn or by the spacing; it is not run for a restart that may go at
    /// once. With the spacing off it takes no turn at all, and the restart runs
    /// as it did before pacing existed.
    pub async fn acquire(&self, on_delay: impl FnOnce()) -> PacedStart<'_> {
        let spacing = role_start_spacing();
        if spacing.is_zero() {
            return PacedStart {
                turn: None,
                wait: Duration::ZERO,
            };
        }
        let mut on_delay = Some(on_delay);
        let turn = match self.last_start.try_lock() {
            Ok(turn) => turn,
            Err(_) => {
                if let Some(on_delay) = on_delay.take() {
                    on_delay();
                }
                self.last_start.lock().await
            }
        };
        let wait = wait_before_start(*turn, Instant::now(), spacing);
        if !wait.is_zero()
            && let Some(on_delay) = on_delay.take()
        {
            on_delay();
        }
        PacedStart {
            turn: Some(turn),
            wait,
        }
    }
}

/// One paced restart's turn ([`RoleStartPacer::acquire`]). Dropping it without
/// [`Self::started`] (the restart failed) records no start.
pub struct PacedStart<'a> {
    turn: Option<tokio::sync::MutexGuard<'a, Option<Instant>>>,
    wait: Duration,
}

impl PacedStart<'_> {
    /// How long this restart must wait before it may begin.
    pub fn wait(&self) -> Duration {
        self.wait
    }

    /// Sleep for [`Self::wait`].
    pub async fn until_due(&self) {
        if !self.wait.is_zero() {
            tokio::time::sleep(self.wait).await;
        }
    }

    /// The restart spawned its agent now; the next paced restart counts its
    /// spacing from here. Ends the turn.
    pub fn started(mut self) {
        if let Some(turn) = self.turn.as_mut() {
            **turn = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACING: Duration = Duration::from_secs(3);

    /// Scenario: a restart is asked for one second after the previous paced
    /// agent started. It waits the remaining two seconds of the spacing.
    #[test]
    fn role_start_pacing_001_a_close_start_waits_the_rest_of_the_spacing() {
        let last = Instant::now();
        let now = last + Duration::from_secs(1);
        assert_eq!(
            wait_before_start(Some(last), now, SPACING),
            Duration::from_secs(2)
        );
    }

    /// Scenario: the first paced restart, and one asked for long after the
    /// previous agent started. Neither waits: the spacing delays only a start
    /// that follows another closely.
    #[test]
    fn role_start_pacing_002_a_lone_start_waits_for_nothing() {
        let now = Instant::now();
        assert_eq!(wait_before_start(None, now, SPACING), Duration::ZERO);
        let last = now;
        let later = now + SPACING * 4;
        assert_eq!(
            wait_before_start(Some(last), later, SPACING),
            Duration::ZERO
        );
    }

    /// Scenario: a restart is asked for while the previous paced agent's start
    /// was exactly one spacing ago. It may start at once.
    #[test]
    fn role_start_pacing_003_a_start_one_spacing_later_waits_for_nothing() {
        let last = Instant::now();
        assert_eq!(
            wait_before_start(Some(last), last + SPACING, SPACING),
            Duration::ZERO
        );
    }

    /// Scenario: a second restart asks for its turn while the first holds it.
    /// It gets its turn only after the first records its start, and then has
    /// to wait a whole spacing from that start.
    #[tokio::test]
    async fn role_start_pacing_004_restarts_take_turns_from_the_previous_start() {
        let pacer = RoleStartPacer::default();
        if role_start_spacing().is_zero() {
            eprintln!("SKIP: {DOT_AGENT_DECK_ROLE_START_SPACING_MS} turns the spacing off here");
            return;
        }
        let first = pacer
            .acquire(|| panic!("the first restart is not delayed"))
            .await;
        assert_eq!(
            first.wait(),
            Duration::ZERO,
            "the first restart waits for nothing"
        );
        let delayed = std::sync::atomic::AtomicBool::new(false);
        let second = pacer.acquire(|| delayed.store(true, std::sync::atomic::Ordering::SeqCst));
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.as_mut())
                .await
                .is_err(),
            "the second restart must not get its turn while the first holds it"
        );
        assert!(
            delayed.load(std::sync::atomic::Ordering::SeqCst),
            "a restart queued behind another is told it is delayed before it waits"
        );
        first.started();
        let second = second.await;
        assert!(
            second.wait() + Duration::from_millis(50) >= role_start_spacing(),
            "the second restart waits a spacing from the first's start, not {:?}",
            second.wait()
        );
    }
}
