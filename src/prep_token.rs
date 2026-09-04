//! Short-lived preparation records for PRD #819's launch verb.
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
//! # What a record binds, and what the original design got wrong
//!
//! **The first cut of this module recorded `(token, issued_at)` and nothing
//! else, and that is not enough — the audit of PRD #819's finished branch found
//! it, and it is a design defect rather than an implementation slip.** The
//! published artifact lives at a path that is **fixed per project**
//! (`<project>/.dot-agent-deck/orchestrator-context.md`), so two ordinary
//! clients preparing in the same project interleave with no attacker involved:
//!
//! 1. preparation A publishes context A and receives token A;
//! 2. preparation B replaces the same fixed file with context B;
//! 3. token A is still inside its TTL, so A's spawn launches a coordinator whose
//!    prompt names that fixed path — and it reads **context B**.
//!
//! A time-only token cannot see any of that. So a record now carries the state
//! its preparation actually approved — [`PrepBinding`] — and every spawn that
//! presents a token re-validates that state before anything is started
//! ([`crate::project_resolve::revalidate_preparation`]). Deleting and recreating
//! the project directory, or changing its config after the preparation, is the
//! same class of mismatch and is refused by the same check.
//!
//! # What a token is, and what it is NOT
//!
//! It **is** a statement that this daemon prepared *this* workflow — this
//! directory, this config revision, this orchestration, these published bytes —
//! within the last [`PREP_TOKEN_TTL`], and that the preparation has not been
//! aged out. That is the entire claim, and it is a **staleness and integrity**
//! claim.
//!
//! It is **not** an authorization token, and must never be treated as one. Any
//! peer that reaches the attach socket already holds the daemon user's local-exec
//! authority through `StartAgent`, which takes arbitrary `command`, `cwd` and
//! `env` — `crate::daemon_protocol::AttachRequest::StartAgent`'s own
//! trust-boundary note says the daemon's job there is "to expose PTY plumbing,
//! not to be a privilege boundary". A peer that wanted to start an arbitrary
//! process does not need a token and is not slowed down by one.
//!
//! **A token is presented on its own verb**,
//! `crate::daemon_protocol::AttachRequest::StartPreparedAgent`, where it is a
//! required field; plain `StartAgent` enforces none and refuses a payload that
//! spells one. Read that split as what it is — the token rode on `StartAgent` as
//! an additive key until the audit follow-up, and an older daemon silently
//! ignored the key and spawned unenforced. Splitting the verb makes such a
//! daemon refuse the request outright. It does **not** make the token an
//! authorization mechanism, and a peer able to call `StartAgent` still needs
//! none.
//!
//! Binding the record to project state is not a step toward authorization and
//! must not be read as one. It stops a *mistake* — a launch consuming state some
//! other launch published — and it stops nothing that a peer able to call
//! `StartAgent` directly could not do anyway. In particular
//! [`PrepBinding::config_revision`] is [`crate::project_resolve::config_revision`],
//! an FNV-1a change **hint** and not a cryptographic commitment; the same is
//! true of [`PrepBinding::context_digest`]. Both detect a change. Neither
//! withstands a deliberately crafted collision, and neither needs to: anyone who
//! can rewrite `.dot-agent-deck.toml` already controls the `command` strings this
//! daemon executes.
//!
//! # The store
//!
//! A process-global queue of `(token, issued_at, binding)`, pruned on every
//! touch and capped at [`MAX_LIVE_PREP_TOKENS`] with oldest-first eviction, so
//! no caller can grow the daemon's memory.
//!
//! **The cap and the queue are process-global, so eviction is NOT per-caller.**
//! An earlier version of this comment claimed "the eviction it causes is of its
//! own earlier tokens", and that was false: the store has no notion of a caller
//! at all, so a client preparing in a loop evicts whatever is oldest — including
//! another client's unspent token. The consequence is bounded and is the same
//! one expiry has: that other launch is refused at its next spawn and has to
//! prepare again. It is not an integrity failure (a refusal is the safe answer,
//! and nothing is launched against the wrong artifact) and it is not a privilege
//! boundary being crossed (see above), but it is a real way for one caller to
//! make another's launch fail, and it is written down here rather than denied.

use std::collections::VecDeque;
use std::path::PathBuf;
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
///
/// It is also not the mechanism that catches the interleaving race the module
/// doc opens with: two preparations seconds apart are both well inside this
/// window, which is exactly why the record binds state and the spawn
/// re-validates it.
pub const PREP_TOKEN_TTL: Duration = Duration::from_secs(120);

/// How many unexpired tokens the daemon keeps.
///
/// **64.** A launch issues one, and no client has any reason to hold more than a
/// handful unspent inside one two-minute window; 64 is far above that and small
/// enough that the store is a fixed, trivial cost. Past it the oldest is
/// evicted, which is the same disposition as expiry arriving early — and the
/// module doc records that the eviction is process-global rather than
/// per-caller.
pub const MAX_LIVE_PREP_TOKENS: usize = 64;

/// Filesystem identity of one inode: `(dev, ino)`.
///
/// The point of carrying it alongside a path is that a path is a *name* and a
/// name can be re-pointed. A `.dot-agent-deck` deleted and recreated, or a
/// project directory replaced between the preparation and the spawn, keeps the
/// same string and gets a new inode — and `rename(2)`, which is how
/// [`crate::orchestrator_context::publish_orchestrator_context`] publishes,
/// *always* installs a new inode over the destination. So for the published
/// context this comparison catches every republish structurally, whatever the
/// bytes say.
///
/// **An inode number is reusable, and the claim is narrowed accordingly.** A
/// directory deleted and recreated at the same path can be handed the number
/// just freed — measured on ext4, often enough that a test written that way
/// passed twice and then failed inside one run. So this comparison does not
/// *prove* an inode was never replaced; what makes that harmless is the
/// **conjunction** the re-validation applies
/// ([`crate::project_resolve::revalidate_preparation`]): the config revision,
/// the published context's inode and its digest all have to coincide too, and if
/// every one of them does then what is on disk is byte-identical to what was
/// approved.
///
/// `None` on a platform where `std` exposes no such identity. That is not a
/// silent degradation of the launch verb's guarantee, because the verb itself is
/// refused there — see
/// [`crate::daemon_protocol::PROJECT_ERR_UNSUPPORTED_PLATFORM`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// The `(dev, ino)` pair of `metadata`, where the platform has one.
pub fn inode_identity(metadata: &std::fs::Metadata) -> Option<InodeIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Some(InodeIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// The state one preparation approved, recorded so a later spawn can prove it
/// is still launching against that same artifact.
///
/// Every field answers a way the world can move between a preparation and the
/// spawn that presents its token. None of them is an authorization claim; read
/// the module doc before treating any of them as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepBinding {
    /// The daemon-canonical project directory the preparation resolved to — the
    /// one string the whole launch uses (`crate::event::PreparedWorkflow::path`).
    pub project_dir: PathBuf,
    /// That directory's inode identity at preparation time, so a delete and
    /// recreate under the same name is caught rather than accepted because the
    /// path still spells the same.
    pub project_identity: Option<InodeIdentity>,
    /// [`crate::project_resolve::config_revision`] of the config bytes the
    /// preparation resolved against. A **change hint**, not a commitment (module
    /// doc).
    pub config_revision: String,
    /// The orchestration name the preparation resolved, as the caller spelled it
    /// and as `crate::project_config::resolve_orchestration_name` rendered it.
    pub orchestration: String,
    /// Where the coordinator context was published.
    pub context_path: PathBuf,
    /// That file's inode identity immediately after the publish.
    pub context_identity: Option<InodeIdentity>,
    /// Digest of the exact bytes published — the platform-independent half of
    /// the same check, and the half that also catches an in-place rewrite that
    /// keeps the inode (a shell `>` redirect, another tool's `fs::write`).
    pub context_digest: String,
}

/// One live record.
#[derive(Debug, Clone)]
struct PrepRecord {
    token: String,
    issued_at: Instant,
    binding: PrepBinding,
}

/// The token store, with the clock as a parameter so the TTL is testable
/// without sleeping.
#[derive(Debug, Default)]
pub struct PrepTokens {
    /// Oldest first, which is what makes both the prune and the eviction a
    /// front-of-queue operation.
    live: VecDeque<PrepRecord>,
}

impl PrepTokens {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a token for `binding` and record it as issued at `now`.
    pub fn issue(&mut self, now: Instant, binding: PrepBinding) -> String {
        self.prune(now);
        while self.live.len() >= MAX_LIVE_PREP_TOKENS {
            self.live.pop_front();
        }
        let token = mint();
        self.live.push_back(PrepRecord {
            token: token.clone(),
            issued_at: now,
            binding,
        });
        token
    }

    /// The binding `token` was issued for, if this store issued it and it has
    /// not aged out by `now`.
    ///
    /// A plain byte comparison, not a constant-time one: the value is not a
    /// secret that authorizes anything (module doc), so a timing side channel
    /// on it leaks the ability to guess a value that grants nothing.
    ///
    /// The token is **not consumed**. One launch presents the same token once
    /// per role, so a one-shot record would refuse every role after the first.
    pub fn binding(&mut self, token: &str, now: Instant) -> Option<PrepBinding> {
        self.prune(now);
        self.live
            .iter()
            .find(|r| r.token == token)
            .map(|r| r.binding.clone())
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
        while let Some(front) = self.live.front() {
            if now.saturating_duration_since(front.issued_at) >= PREP_TOKEN_TTL {
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

/// Issue a token for `binding` from the daemon-wide store.
pub fn issue(binding: PrepBinding) -> String {
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    guard.issue(Instant::now(), binding)
}

/// The binding `token` was issued for, if this daemon issued it within
/// [`PREP_TOKEN_TTL`].
///
/// `None` is the one answer for "not ours" and "aged out" alike — they are the
/// same fact to a caller: the token does not identify a live preparation.
pub fn binding(token: &str) -> Option<PrepBinding> {
    let mut guard = store().lock().unwrap_or_else(|p| p.into_inner());
    guard.binding(token, Instant::now())
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

    fn binding_for(dir: &str) -> PrepBinding {
        PrepBinding {
            project_dir: PathBuf::from(dir),
            project_identity: Some(InodeIdentity { dev: 1, ino: 2 }),
            config_revision: "fnv1a128-00".to_string(),
            orchestration: "loop".to_string(),
            context_path: PathBuf::from(dir).join(".dot-agent-deck/orchestrator-context.md"),
            context_identity: Some(InodeIdentity { dev: 1, ino: 3 }),
            context_digest: "ctx-fnv1a128-00".to_string(),
        }
    }

    /// A token is valid the moment it is issued and for the whole TTL.
    #[test]
    fn a_fresh_token_validates_for_the_whole_ttl() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let token = tokens.issue(now, binding_for("/p"));
        assert!(tokens.binding(&token, now).is_some());
        assert!(
            tokens
                .binding(&token, now + PREP_TOKEN_TTL - Duration::from_millis(1))
                .is_some()
        );
    }

    /// The point of the TTL: a preparation that has aged out is refused, and it
    /// is refused the same way an unknown value is — there is one answer, not a
    /// distinguishable pair.
    #[test]
    fn an_expired_token_is_refused() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let token = tokens.issue(now, binding_for("/p"));
        assert!(
            tokens.binding(&token, now + PREP_TOKEN_TTL).is_none(),
            "a token exactly at the TTL has aged out"
        );
        assert!(
            tokens
                .binding(&token, now + PREP_TOKEN_TTL + Duration::from_secs(1))
                .is_none()
        );
        assert!(tokens.is_empty(), "an expired token is dropped, not kept");
    }

    #[test]
    fn an_unknown_token_is_refused() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let issued = tokens.issue(now, binding_for("/p"));
        assert!(tokens.binding("prep-not-one-we-issued", now).is_none());
        assert!(tokens.binding("", now).is_none());
        // And the real one still validates, so the refusal above is about the
        // value rather than about the store being broken.
        assert!(tokens.binding(&issued, now).is_some());
    }

    /// The record hands back the state its preparation approved, and two
    /// preparations in the same project get two different records rather than
    /// one shared one. This is the store-side half of the interleaving fix: the
    /// spawn-side half is
    /// `project_resolve::revalidate_preparation`.
    #[test]
    fn each_token_carries_its_own_binding() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();

        let mut first = binding_for("/p");
        first.context_digest = "ctx-fnv1a128-aaaa".to_string();
        let mut second = binding_for("/p");
        second.context_digest = "ctx-fnv1a128-bbbb".to_string();

        let token_a = tokens.issue(now, first.clone());
        let token_b = tokens.issue(now, second.clone());

        assert_eq!(tokens.binding(&token_a, now), Some(first));
        assert_eq!(tokens.binding(&token_b, now), Some(second));
    }

    /// Presenting a token does not consume it: one launch presents the same
    /// token once per role, so a one-shot record would refuse every role after
    /// the first.
    #[test]
    fn a_token_is_reusable_within_its_ttl() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let token = tokens.issue(now, binding_for("/p"));
        for _ in 0..5 {
            assert!(tokens.binding(&token, now).is_some());
        }
        assert_eq!(tokens.len(), 1);
    }

    /// A caller that prepares in a loop cannot grow the daemon's memory: the
    /// store is capped and evicts oldest-first.
    ///
    /// It is deliberately asserted with the FIRST token belonging to a
    /// *different* preparation than the loop's, because the cap and the queue
    /// are process-global — the module doc says so, and this pins it rather
    /// than leaving the old "it only evicts its own" claim standing.
    #[test]
    fn the_store_is_capped_and_evicts_oldest_first_across_callers() {
        let mut tokens = PrepTokens::new();
        let now = Instant::now();
        let someone_elses = tokens.issue(now, binding_for("/other-project"));
        for _ in 0..MAX_LIVE_PREP_TOKENS {
            tokens.issue(now, binding_for("/looping-caller"));
        }
        assert_eq!(tokens.len(), MAX_LIVE_PREP_TOKENS);
        assert!(
            tokens.binding(&someone_elses, now).is_none(),
            "the oldest token is the one evicted, whoever issued it"
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
        let expected = binding_for("/global");
        let token = issue(expected.clone());
        assert_eq!(binding(&token), Some(expected));
        assert!(binding("prep-0000000000000000000000000000000").is_none());
    }
}
