//! Short-lived preparation tokens for PRD #819's launch verb.
//!
//! # Why this exists at all
//!
//! [`crate::daemon_protocol::AttachRequest::PrepareWorkflow`] resolves a
//! project, composes the coordinator context and publishes it in one operation.
//! **Spawning is not part of that operation** — PRD #819 Open Question 5 was
//! settled toward the smaller change, so the roles are still started by a later
//! sequence of [`crate::daemon_protocol::AttachRequest::StartAgent`] calls.
//! Without something bridging that gap, "resolve and write atomically" is a
//! claim the design does not deliver: the config or the path can be replaced
//! between the picker, the write and the spawn, and nothing notices.
//!
//! Two mechanisms close that window and they close different halves of it. The
//! **config revision** ([`crate::project_resolve::config_revision`]) catches the
//! config changing between a resolve and the prepare that follows it. The token
//! here catches the *prepare* being stale by the time a spawn presents it.
//!
//! # What a token is, and what it is NOT
//!
//! It **is** a statement that this daemon prepared a workflow within the last
//! [`PREP_TOKEN_TTL`], and that the preparation has not been aged out. That is
//! the entire claim.
//!
//! It is **not** an authorization token, and must never be treated as one. Any
//! peer that reaches the attach socket already holds the daemon user's local-exec
//! authority through `StartAgent`, which takes arbitrary `command`, `cwd` and
//! `env` — `crate::daemon_protocol::AttachRequest::StartAgent`'s own
//! trust-boundary note says the daemon's job there is "to expose PTY plumbing,
//! not to be a privilege boundary". A peer that wanted to start an arbitrary
//! process does not need a token and is not slowed down by one. Presenting a
//! token is therefore **optional** on `StartAgent`, and an absent one leaves that
//! verb behaving exactly as it does today.
//!
//! It does not bind a token to a project, an orchestration or a role either.
//! Binding would be a larger change on the `StartAgent` side than the window it
//! closes justifies, and pretending otherwise in a comment is how a convenience
//! gets read as a control later.
//!
//! # The store
//!
//! A process-global set of `(token, issued_at)`, pruned on every touch and
//! capped at [`MAX_LIVE_PREP_TOKENS`] with oldest-first eviction — so a caller
//! that prepares in a loop cannot grow the daemon's memory, and the eviction it
//! causes is of its own earlier tokens.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a preparation token stays valid.
///
/// **Two minutes.** The window it has to cover is one client's prepare → spawn
/// sequence: the desktop issues `PrepareWorkflow` and then one `StartAgent` per
/// role from the same action handler, which is sub-second in the ordinary case
/// and a few seconds if a PTY spawn is slow. Two minutes is three orders of
/// magnitude of headroom over that, so the TTL never turns a slow launch into a
/// mysterious refusal.
///
/// And the other direction, stated as narrowly as it is true: the TTL bounds how
/// long a *stale* preparation stays presentable at two minutes rather than at
/// the daemon's lifetime. It is not a defence against a peer that can read the
/// token — that peer can call `StartAgent` with no token at all (see the module
/// doc), so shortening it further would buy nothing and cost robustness.
pub const PREP_TOKEN_TTL: Duration = Duration::from_secs(120);

/// How many unexpired tokens the daemon keeps.
///
/// **64.** A launch issues one, and no client has any reason to hold more than a
/// handful unspent inside one two-minute window; 64 is far above that and small
/// enough that the store is a fixed, trivial cost. Past it the oldest is
/// evicted, which is the same disposition as expiry arriving early.
pub const MAX_LIVE_PREP_TOKENS: usize = 64;

/// The token store, with the clock as a parameter so the TTL is testable
/// without sleeping.
#[derive(Debug, Default)]
pub struct PrepTokens {
    /// Oldest first, which is what makes both the prune and the eviction a
    /// front-of-queue operation.
    live: VecDeque<(String, Instant)>,
}

impl PrepTokens {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a token and record it as issued at `now`.
    pub fn issue(&mut self, now: Instant) -> String {
        self.prune(now);
        while self.live.len() >= MAX_LIVE_PREP_TOKENS {
            self.live.pop_front();
        }
        let token = mint();
        self.live.push_back((token.clone(), now));
        token
    }

    /// Whether `token` was issued by this store and has not aged out by `now`.
    ///
    /// A plain byte comparison, not a constant-time one: the value is not a
    /// secret that authorizes anything (module doc), so a timing side channel
    /// on it leaks the ability to guess a value that grants nothing.
    pub fn is_valid(&mut self, token: &str, now: Instant) -> bool {
        self.prune(now);
        self.live.iter().any(|(t, _)| t == token)
    }

    /// How many unexpired tokens the store holds. Test-facing.
    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    fn prune(&mut self, now: Instant) {
        // `saturating_duration_since` rather than subtraction: `now` is supplied
        // by the caller, and a value behind an entry's `issued_at` must read as
        // "no time has passed" rather than panicking.
        while let Some((_, issued)) = self.live.front() {
            if now.saturating_duration_since(*issued) >= PREP_TOKEN_TTL {
                self.live.pop_front();
            } else {
                break;
            }
        }
    }
}

fn store() -> &'static Mutex<PrepTokens> {
    static STORE: OnceLock<Mutex<PrepTokens>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(PrepTokens::new()))
}

/// Issue a token from the daemon-wide store.
pub fn issue() -> String {
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    guard.issue(Instant::now())
}

/// Whether `token` is one this daemon issued within [`PREP_TOKEN_TTL`].
pub fn is_valid(token: &str) -> bool {
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    guard.is_valid(token, Instant::now())
}

/// Mint 128 bits of token value.
///
/// [`std::hash::RandomState`] is seeded from the operating system's randomness
/// once per thread and then incremented per instance, so two fresh ones plus a
/// process-wide counter give a value that is not guessable from outside the
/// process. That is the level of unpredictability this value warrants and no
/// more: it is not an authorization token (module doc), so what matters is that
/// two tokens never collide, and a random 128-bit value delivers that without a
/// new dependency.
fn mint() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut high = RandomState::new().build_hasher();
    high.write_u64(seq);
    high.write_u64(std::process::id() as u64);
    let high = high.finish();
    let mut low = RandomState::new().build_hasher();
    low.write_u64(high);
    low.write_u64(seq);
    format!("prep-{high:016x}{:016x}", low.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token is valid the moment it is issued and for the whole TTL.
    #[test]
    fn a_fresh_token_validates_for_the_whole_ttl() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let token = tokens.issue(now);
        assert!(tokens.is_valid(&token, now));
        assert!(tokens.is_valid(&token, now + PREP_TOKEN_TTL - Duration::from_millis(1)));
    }

    /// The point of the TTL: a preparation that has aged out is refused, and it
    /// is refused the same way an unknown value is — there is one answer, not a
    /// distinguishable pair.
    #[test]
    fn an_expired_token_is_refused() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let token = tokens.issue(now);
        assert!(
            !tokens.is_valid(&token, now + PREP_TOKEN_TTL),
            "a token exactly at the TTL has aged out"
        );
        assert!(!tokens.is_valid(&token, now + PREP_TOKEN_TTL + Duration::from_secs(1)));
        assert!(tokens.is_empty(), "an expired token is dropped, not kept");
    }

    #[test]
    fn an_unknown_token_is_refused() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let issued = tokens.issue(now);
        assert!(!tokens.is_valid("prep-not-one-we-issued", now));
        assert!(!tokens.is_valid("", now));
        // And the real one still validates, so the refusal above is about the
        // value rather than about the store being broken.
        assert!(tokens.is_valid(&issued, now));
    }

    /// A caller that prepares in a loop cannot grow the daemon's memory: the
    /// store is capped and evicts oldest-first.
    #[test]
    fn the_store_is_capped_and_evicts_oldest_first() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let first = tokens.issue(now);
        for _ in 0..MAX_LIVE_PREP_TOKENS {
            tokens.issue(now);
        }
        assert_eq!(tokens.len(), MAX_LIVE_PREP_TOKENS);
        assert!(
            !tokens.is_valid(&first, now),
            "the oldest token is the one evicted"
        );
    }

    /// Two tokens must never collide, or one launch's staleness check would
    /// silently accept another's.
    #[test]
    fn minted_tokens_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(mint()), "mint() produced a duplicate");
        }
    }

    /// The process-global entry points agree with the store they wrap.
    #[test]
    fn the_global_store_round_trips() {
        let token = issue();
        assert!(is_valid(&token));
        assert!(!is_valid("prep-0000000000000000000000000000000"));
    }
}
