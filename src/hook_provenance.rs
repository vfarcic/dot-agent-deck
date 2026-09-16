//! Provenance for hook-socket messages: tying a signal to the pane it claims to
//! come from (issue #1077).
//!
//! # The problem
//!
//! Every [`crate::event::DaemonMessage`] names a `pane_id`, and until this
//! module existed that name was the *whole* of the claim. The first-party CLI
//! copies `DOT_AGENT_DECK_PANE_ID` into the field and the daemon acts on it, so
//! the authority a message carries rested on an environment variable. Two
//! consequences, both pre-existing and both reachable without an attacker:
//! `AppState::handle_delegate_with_state` writes and submits a task prompt into
//! **every worker pane** of the orchestration whose orchestrator pane is named,
//! and `AppState::handle_work_done` writes feedback into the **orchestrator's**
//! pane — a different pane from the one the message claimed to come from. PRD
//! #220's dispatch return route adds a third: `take_dispatch_return` is
//! one-shot, so a forged completion *spends* the route and the real unit's later
//! completion resolves to nothing.
//!
//! The socket's owner-only permissions exclude other OS users. They do not
//! separate the several agents this deck deliberately runs under **one** uid,
//! and those agents can read pane ids straight off the daemon's own
//! `list-agents` / `daemon status` surface.
//!
//! # What this module does
//!
//! The daemon mints 256 bits of OS randomness per spawn ([`mint`]), keeps it on
//! the agent's registry record, and injects it into the child's environment as
//! [`DOT_AGENT_DECK_PANE_CAPABILITY`] beside the pane id. The CLI presents it on every
//! hook-socket message, and the daemon resolves **token → record → pane** and
//! refuses a message whose claimed pane is not the one the token was minted for
//! ([`classify`]).
//!
//! The direction of that lookup is the point. The token is not an extra field
//! checked against the claim; it *is* the claim, and the `pane_id` on the wire
//! is checked against it. So an agent holding a perfectly valid token of its own
//! still cannot name a sibling's pane — [`Refusal::WrongPane`].
//!
//! # What it is NOT — read this before citing it as a fix
//!
//! **It is not a boundary against a deliberate same-uid adversary, and it cannot
//! be.** The token has to be readable by the legitimate sender, the sender is a
//! process, and on Linux one process's environment is readable by any process of
//! the same uid through `/proc/<pid>/environ`. A sibling agent that reads a
//! victim's environ gets the victim's token and can then forge everything this
//! module checks. Nothing in the deck's reach changes that: any mechanism whose
//! secret is delivered to the agent falls the same way, and the alternatives
//! that do not deliver a secret are discussed below and do not work either.
//!
//! What it *does* do, stated no wider than it is true:
//!
//! - **It removes the published escalation.** Pane ids are advertised — by
//!   `list-agents`, by `daemon status`, in the TUI. The daemon does not publish
//!   the token: it is absent from [`crate::agent_pty::AgentRecord`] (the wire
//!   projection clients receive) and the daemon never logs it. (An agent that
//!   prints its own environment puts its token into its pane's scrollback,
//!   which an attach client can snapshot — a leak by the agent, not by the
//!   daemon, and one more reason this is not a boundary.) So "read a pane id off the
//!   daemon's own status output and forge a message" — the path issue #1077
//!   describes — no longer reaches anything. The adversary now needs a
//!   *different* capability — reading another process's environment — which is
//!   a deliberate act rather than something an agent does by accident. It is not
//!   a hard one: measured on the development box this was written on, with YAMA
//!   `ptrace_scope = 1`, a same-uid process that is neither ancestor nor
//!   descendant of the target could still read its `/proc/<pid>/environ` and
//!   find the `DOT_AGENT_DECK_*` keys there. (`hidepid=` does not change that
//!   either: it hides *other* users' processes, not the same user's.)
//! - **It binds a message to one spawn.** A token is re-minted on every spawn
//!   and every respawn, so an environment that outlives its pane — a stale
//!   `DOT_AGENT_DECK_PANE_ID` inherited by an unrelated process, a recycled pane
//!   id, a survivor of a daemon restart — authorises nothing. That half is a
//!   correctness property, not a security one, and it is the half that catches
//!   the *accidental* forgeries, which are the ones that actually happen.
//!
//! The same-uid residual is issue #1129. Issues #543 and #401 stay open too:
//! raw [`crate::event::AgentEvent`] traffic
//! (the hook scripts installed into each agent's own config, plus the
//! `agent-event` verb) is deliberately **out of scope** here — see the module
//! docs on [`classify`] for why, and `docs/develop/hook-provenance.md` for the
//! whole threat model.
//!
//! # Why not `SO_PEERCRED`
//!
//! It is the obvious reach and it does not answer this question.
//!
//! - **The uid is the wrong discriminator.** `SO_PEERCRED` reports the peer's
//!   uid, and every agent this deck runs has the *same* uid — that is the entire
//!   threat. The socket is already `0600`
//!   (`crate::platform::fsperm`), so a uid check re-states a guarantee the
//!   filesystem already gives and separates nothing inside it.
//! - **The pid cannot be resolved to a pane in time.** The peer pid *is*
//!   unforgeable, and in principle a walk up the process tree could say which
//!   pane's PTY the sender lives under. But `work-done`, `dispatch` and the raw
//!   event path are fire-and-forget: the CLI connects, writes one line and
//!   exits. By the time the daemon has the line, the process that sent it is
//!   routinely **gone**, so the walk has nothing to read. A check that answers
//!   "cannot tell" for a legitimate sender has to fail open, and a check that
//!   fails open is not a check — an adversary simply exits first.
//! - **Even when it answers, it is weaker than the token.** The walk costs a
//!   `ps` sample (`crate::platform::proc::process_table`, ~12 ms of fork on this
//!   box) on every message, is defeated by `setsid` and by re-parenting to init,
//!   and a same-uid adversary controls its own process tree. It would buy a
//!   slower, flakier check that the same adversary defeats more easily.
//!
//! So peer credentials are deliberately not used. They are not dismissed as
//! irrelevant — they are the right tool for `daemon stop`'s "which pid is
//! serving this socket", which is exactly what [`crate::platform::peercred`]
//! already uses them for.

use std::sync::atomic::{AtomicU64, Ordering};

/// Per-spawn hook capability token the daemon injects into every agent it
/// spawns, beside [`crate::agent_pty::DOT_AGENT_DECK_PANE_ID`] and
/// [`crate::agent_pty::DOT_AGENT_DECK_AGENT_ID`].
///
/// Same drift-safety pattern as those two: the constant is defined once and the
/// spawn-side injector, the env-scrub site and the CLI readers all reference
/// this symbol, so two string literals cannot drift apart.
///
/// A caller-supplied value is **stripped** at the spawn seam rather than
/// honoured — unlike [`crate::agent_pty::DOT_AGENT_DECK_SOCKET`], where the
/// injection only fills a gap. Honouring one would let two records carry the
/// same token, and [`classify`]'s token → record resolution has no answer for
/// that: whichever record the scan reached first would decide which pane a
/// message is allowed to name.
///
/// **The name deliberately contains none of `KEY`, `SECRET` or `TOKEN`**, and
/// that is load-bearing, not taste. Agents scrub environment variables with
/// those words in their names out of the shell their model runs commands in:
/// Codex's `shell_environment_policy` drops every name matching `*KEY*`,
/// `*SECRET*` or `*TOKEN*` (case-insensitive) whenever
/// `ignore_default_excludes = false` — a documented hardening switch (checked
/// against `openai/codex` `codex-rs/protocol/src/shell_environment.rs`, whose
/// TOML default is `true`). Named `…_HOOK_TOKEN`, as the first cut of this was,
/// the value would vanish for exactly the users who enabled that switch, their
/// Codex workers' `work-done` would arrive with no token, and the daemon would
/// refuse it — invisibly, because `work-done` reads no reply. The capability has
/// to reach the command the agent runs, which is precisely what those filters
/// exist to stop for API credentials; `DOT_AGENT_DECK_PANE_ID` already passes
/// them for the same reason. `hook_provenance::tests::
/// the_capability_variable_survives_agents_secret_name_filters` pins it.
pub const DOT_AGENT_DECK_PANE_CAPABILITY: &str = "DOT_AGENT_DECK_PANE_CAPABILITY";

/// Operator override for what the daemon does with a message that presents **no**
/// token for a pane that was issued one — see [`Policy`].
pub const DOT_AGENT_DECK_HOOK_PROVENANCE: &str = "DOT_AGENT_DECK_HOOK_PROVENANCE";

/// Bytes of randomness behind a token. 32 bytes = 256 bits, rendered as 64
/// lowercase hex characters.
const TOKEN_BYTES: usize = 32;

/// The exact length of a well-formed token, in characters.
pub const TOKEN_LEN: usize = TOKEN_BYTES * 2;

/// Mint one hook capability token: [`TOKEN_BYTES`] bytes straight from the
/// operating system's randomness, hex-encoded.
///
/// **`getrandom` rather than [`std::hash::RandomState`]**, which is what
/// [`crate::prep_token::issue`] uses and which would have cost no new
/// dependency. That module's own doc says why it is not enough here: its value
/// is "not guessable from outside the process", which it earns by hashing under
/// a per-thread key seeded once from the OS *and then incremented per instance*.
/// Two tokens never colliding is all a preparation record needs. This value is a
/// capability — an adversary who obtains one must learn nothing about the next —
/// so it takes its randomness from the OS directly. `getrandom` is already in
/// this workspace's dependency graph, so naming it adds no new code to the
/// build.
///
/// The counter is a **collision** backstop only, mixed in so that a
/// catastrophically broken `getrandom` (one returning a constant) still yields
/// distinct tokens rather than one token that attests every pane. It adds no
/// unpredictability and is not relied on for any.
pub fn mint() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; TOKEN_BYTES];
    // A failure here is not recoverable into a weaker token: the value's whole
    // job is to be unguessable, and a fallback that is guessable would be worse
    // than the panic because it would look identical from every call site. On
    // every platform this project builds for, `getrandom` fails only if the OS
    // randomness source is itself unavailable.
    getrandom::fill(&mut bytes).expect("OS randomness is unavailable; cannot mint a hook token");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed).to_le_bytes();
    for (slot, counter) in bytes[TOKEN_BYTES - seq.len()..].iter_mut().zip(seq) {
        *slot ^= counter;
    }
    let mut out = String::with_capacity(TOKEN_LEN);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Whether `candidate` has the shape [`mint`] produces.
///
/// Checked before any lookup so a peer cannot make the daemon scan its registry
/// against a megabyte of attacker-chosen text, and so a malformed value is
/// refused as malformed rather than as "unknown token", which reads very
/// differently in a log.
pub fn is_well_formed(candidate: &str) -> bool {
    candidate.len() == TOKEN_LEN && candidate.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Compare two tokens without an early return on the first differing byte.
///
/// A timing oracle is **not** the threat this module is built against — a peer
/// on this socket already has local code execution as the user, and the far
/// cheaper attack is reading `/proc/<pid>/environ` (module docs). This is here
/// because the cost is six lines and its absence is a finding every reviewer of
/// a token comparison is right to raise.
pub(crate) fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The record a minted token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenOwner {
    /// The registry agent id — the `DOT_AGENT_DECK_AGENT_ID` of the same spawn.
    pub agent_id: String,
    /// The `DOT_AGENT_DECK_PANE_ID` that spawn was tagged with, if any. A
    /// daemon-spawned agent without a pane id is a supported shape, and a token
    /// minted for one can never attest a pane claim.
    pub pane_id: Option<String>,
}

/// What the daemon's registry has to answer for [`classify`] to decide.
///
/// A trait rather than a direct dependency on
/// [`crate::agent_pty::AgentPtyRegistry` ] so the decision is testable without
/// spawning a PTY: the whole policy is exercised against a table of pairs, and
/// the registry supplies the same two answers in production.
pub trait HookTokenDirectory {
    /// The record this daemon minted `token` for, if it minted it at all.
    ///
    /// Exited records are included deliberately. An agent that has detached from
    /// the PTY it was born under outlives its record's live flag and can still
    /// signal; refusing it because its record is flagged `exited` would turn a
    /// survivor into a forger. The token still names exactly one spawn either
    /// way, which is the property the check rests on.
    fn owner_of_hook_token(&self, token: &str) -> Option<TokenOwner>;

    /// Whether this daemon has **ever** issued a token for `pane_id`.
    ///
    /// This is what makes a *missing* token distinguishable from a pane the
    /// daemon never spawned. It must be answered from something that outlives
    /// any one record: a pane is briefly without a record during a respawn while
    /// its role maps — and so its authority — survive, and an answer derived from
    /// the live records would read that window as "never issued" and admit a
    /// token-less forgery into it.
    fn pane_was_issued_a_hook_token(&self, pane_id: &str) -> bool;
}

/// Why a message was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The presented value is not shaped like a token this daemon mints.
    Malformed,
    /// A well-formed token this daemon has no record of. The ordinary cause is a
    /// token minted by a **previous** daemon, held by an agent that survived the
    /// restart; the other cause is a guess.
    UnknownToken,
    /// A token this daemon minted, presented alongside a pane id it was not
    /// minted for. This is the forgery shape that a token-bearing sibling would
    /// use, and the reason the lookup runs token → pane rather than the other
    /// way round.
    WrongPane {
        /// The pane the token actually names, for the log line. `None` when the
        /// token's spawn carried no pane id.
        token_pane: Option<String>,
    },
    /// No token at all, for a pane that was issued one.
    Missing,
}

/// The verdict on one hook-socket message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// The token names exactly the pane the message claims.
    Attested {
        /// The registry agent id the token was minted for. Equal to the agent
        /// the claimed pane is occupied by, since the token names one spawn.
        agent_id: String,
    },
    /// This daemon has no record of the claimed pane at all, so there is nothing
    /// to check the message against.
    ///
    /// Why this branch is not the obvious hole, stated because it is the branch
    /// a reader reaches for first. It is reached only for a pane id this daemon
    /// has **never** issued a token for — not merely one that has no record at
    /// this instant (see
    /// [`HookTokenDirectory::pane_was_issued_a_hook_token`]). In the daemon, the
    /// role maps that carry delegate and work-done authority — `pane_role_map`
    /// and `orchestrator_pane_ids` — are written only by
    /// `AppState::register_orchestration_role`, and each of its five callers
    /// (the `StartAgent` handler, the scheduler/dispatch orchestration spawn,
    /// `spawn_role`, and the two respawn-or-recreate paths) calls it only after
    /// its spawn returned `Ok`. Every spawn records its pane as issued before it
    /// forks. So no pane that lands here holds a role. That was checked by
    /// enumerating the write sites when this was written; a new writer of those
    /// maps that does not go through a spawn would break it. The residual it
    /// leaves is `AppState::apply_event`'s auto-registration of an unknown
    /// `SessionStart`, which gives a CARD to a pane nobody spawned but no role,
    /// and is issue #543's surface rather than this one's.
    Unattested,
    /// Refused; the message must not be acted on.
    Refused(Refusal),
}

/// What the daemon does with [`Refusal::Missing`] — and *only* with that one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Refuse it. The default.
    Enforce,
    /// Act on it, and warn. The escape hatch for exactly one situation: the
    /// `dot-agent-deck` binary invoked inside a pane is **older** than the
    /// daemon that spawned that pane, so it does not know to forward the token
    /// the daemon put in its environment. That is a mixed-install condition
    /// (a daemon started from a build other than the one on `PATH`), it is the
    /// only way a legitimate sender produces [`Refusal::Missing`], and the
    /// refusal log line names this variable so the person who hits it can act on
    /// it.
    ///
    /// It deliberately does **not** relax [`Refusal::UnknownToken`] or
    /// [`Refusal::WrongPane`]. Neither can be produced by an old CLI — an old
    /// CLI sends no token at all — so tolerating them would buy compatibility
    /// with nothing and would let the escape hatch re-open the forgery it exists
    /// beside.
    WarnOnly,
}

impl Policy {
    /// Resolve the policy from a raw environment value.
    ///
    /// Anything other than an exact, case-insensitive `warn` is [`Enforce`](Policy::Enforce),
    /// including a typo. A knob that weakens a check must not be switchable by
    /// accident, and an unrecognised value meaning "the strict thing" is the
    /// direction that fails safe.
    pub fn from_env_value(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("warn") => Policy::WarnOnly,
            _ => Policy::Enforce,
        }
    }
}

/// Resolve the policy from [`DOT_AGENT_DECK_HOOK_PROVENANCE`].
pub fn policy() -> Policy {
    Policy::from_env_value(
        std::env::var(DOT_AGENT_DECK_HOOK_PROVENANCE)
            .ok()
            .as_deref(),
    )
}

/// Decide whether a hook-socket message claiming `claimed_pane` may be acted on.
///
/// `presented` is the token the message carried, `None` when it carried none.
///
/// # Scope: `DaemonMessage` only
///
/// Every variant of [`crate::event::DaemonMessage`] names a pane and every one
/// of them goes through here. Raw [`crate::event::AgentEvent`] traffic — the
/// other thing this socket accepts — does **not**, and that exclusion is
/// deliberate rather than an oversight:
///
/// - An `AgentEvent` is a **published schema** (PRD #20's
///   `AGENT_EVENT_SCHEMA_VERSION`) that third-party producers emit. Requiring a
///   field on it breaks producers this project does not ship.
/// - The events arrive from hook scripts already written into each agent's own
///   configuration — `~/.claude/settings.json`, `~/.codex/config.toml` and the
///   rest — by installations that predate this change. Refusing a token-less
///   event would break every existing hook installation on upgrade.
/// - `AppState::apply_event` auto-registers an unknown `SessionStart` to cover a
///   startup race, so the blast radius of a refusal there is the whole card
///   surface rather than one orchestration.
///
/// Issues #543 and #401 track that half and stay open. This function closes the
/// verbs that **write into another pane, spawn, or consume a one-shot route**;
/// it does not close status reporting.
pub fn classify(
    claimed_pane: &str,
    presented: Option<&str>,
    directory: &impl HookTokenDirectory,
) -> Provenance {
    let Some(token) = presented else {
        return if directory.pane_was_issued_a_hook_token(claimed_pane) {
            Provenance::Refused(Refusal::Missing)
        } else {
            Provenance::Unattested
        };
    };
    if !is_well_formed(token) {
        return Provenance::Refused(Refusal::Malformed);
    }
    let Some(owner) = directory.owner_of_hook_token(token) else {
        return Provenance::Refused(Refusal::UnknownToken);
    };
    match owner.pane_id.as_deref() {
        Some(pane) if pane == claimed_pane => Provenance::Attested {
            agent_id: owner.agent_id,
        },
        other => Provenance::Refused(Refusal::WrongPane {
            token_pane: other.map(str::to_string),
        }),
    }
}

/// Whether a [`Provenance`] permits the message to be acted on under `policy`,
/// and the refusal to report when it does not.
///
/// Returns `Ok(())` to proceed. `Err(refusal)` is the one that must be refused —
/// note that [`Refusal::Missing`] under [`Policy::WarnOnly`] returns `Ok(())`,
/// which is the entire difference between the two policies.
pub fn admits(provenance: &Provenance, policy: Policy) -> Result<(), Refusal> {
    match provenance {
        Provenance::Attested { .. } | Provenance::Unattested => Ok(()),
        Provenance::Refused(Refusal::Missing) if policy == Policy::WarnOnly => Ok(()),
        Provenance::Refused(refusal) => Err(refusal.clone()),
    }
}

impl Refusal {
    /// A stable, greppable word for logs and for the `error` field of the verbs
    /// that answer on the same connection.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Malformed => "malformed_token",
            Refusal::UnknownToken => "unknown_token",
            Refusal::WrongPane { .. } => "token_names_another_pane",
            Refusal::Missing => "missing_token",
        }
    }

    /// One sentence for the caller that asked, naming the remedy where there is
    /// one.
    ///
    /// Deliberately says nothing the sender did not already know: it never
    /// echoes the presented token, and it never reveals which pane a token
    /// actually belongs to, so a refusal cannot be used to enumerate the
    /// daemon's panes.
    pub fn caller_message(&self) -> String {
        match self {
            Refusal::Malformed | Refusal::UnknownToken => {
                "refused: this pane's hook capability token is not one this daemon issued. \
                 A token is minted per spawn, so an agent that outlived the daemon that \
                 started it has to be restarted."
                    .to_string()
            }
            Refusal::WrongPane { .. } => {
                "refused: this pane's hook capability token was issued for a different pane, \
                 so the pane named by this message is not the one it came from."
                    .to_string()
            }
            Refusal::Missing => format!(
                "refused: this pane was issued a hook capability token and the message \
                 presented none. The usual cause is that the `dot-agent-deck` binary invoked \
                 in this pane is older than the daemon that spawned it; set \
                 {DOT_AGENT_DECK_HOOK_PROVENANCE}=warn on the daemon to accept it anyway."
            ),
        }
    }
}

/// The hook capability token this process inherited, if it was spawned by a
/// daemon that mints them.
///
/// `None` is "this process was not given one" and never an empty string: an
/// empty value would present as a token, be refused as malformed, and read in a
/// log as a forgery rather than as an environment that never carried one.
pub fn token_from_env() -> Option<String> {
    std::env::var(DOT_AGENT_DECK_PANE_CAPABILITY)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A stand-in registry: token → (agent id, pane).
    ///
    /// The whole policy is decided from these two answers, so the matrix below
    /// is a complete test of [`classify`] without a PTY anywhere near it. The
    /// real [`crate::agent_pty::AgentPtyRegistry`] supplies the same two and is
    /// exercised over a real socket in `crate::daemon`'s hook-loop tests.
    struct Stub {
        by_token: HashMap<String, (String, Option<String>)>,
    }

    impl Stub {
        fn new(rows: &[(&str, &str, Option<&str>)]) -> Self {
            Self {
                by_token: rows
                    .iter()
                    .map(|(t, a, p)| ((*t).to_string(), ((*a).to_string(), p.map(str::to_string))))
                    .collect(),
            }
        }
    }

    impl HookTokenDirectory for Stub {
        fn owner_of_hook_token(&self, token: &str) -> Option<TokenOwner> {
            self.by_token
                .get(token)
                .map(|(agent_id, pane_id)| TokenOwner {
                    agent_id: agent_id.clone(),
                    pane_id: pane_id.clone(),
                })
        }

        fn pane_was_issued_a_hook_token(&self, pane_id: &str) -> bool {
            self.by_token
                .values()
                .any(|(_, p)| p.as_deref() == Some(pane_id))
        }
    }

    /// A syntactically valid token that is not in any fixture.
    const STRANGER: &str = "ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00ff00";

    fn fixture() -> Stub {
        Stub::new(&[
            ("aa".repeat(32).leak(), "agent-1", Some("pane-orchestrator")),
            ("bb".repeat(32).leak(), "agent-2", Some("pane-worker")),
            // A daemon-spawned agent with no pane id at all — a supported shape
            // (`RunningAgent::pane_id_env` is an `Option`), and one whose token
            // must never attest anybody's pane.
            ("cc".repeat(32).leak(), "agent-3", None),
        ])
    }

    #[test]
    fn a_token_attests_its_own_pane() {
        assert_eq!(
            classify("pane-orchestrator", Some(&"aa".repeat(32)), &fixture()),
            Provenance::Attested {
                agent_id: "agent-1".to_string()
            }
        );
    }

    /// THE forgery this whole module exists for: a caller that holds a perfectly
    /// valid token of its own, naming a sibling's pane. It is the shape a
    /// same-uid agent reaches for first, because its own token is the one thing
    /// it certainly has, and it is the reason the lookup runs token → pane
    /// rather than checking a token against the pane the message named.
    #[test]
    fn a_valid_token_cannot_name_another_pane() {
        assert_eq!(
            classify("pane-orchestrator", Some(&"bb".repeat(32)), &fixture()),
            Provenance::Refused(Refusal::WrongPane {
                token_pane: Some("pane-worker".to_string())
            })
        );
    }

    #[test]
    fn a_token_minted_for_a_pane_less_agent_attests_nothing() {
        assert_eq!(
            classify("pane-worker", Some(&"cc".repeat(32)), &fixture()),
            Provenance::Refused(Refusal::WrongPane { token_pane: None })
        );
    }

    #[test]
    fn a_token_this_daemon_never_minted_is_refused() {
        assert_eq!(
            classify("pane-worker", Some(STRANGER), &fixture()),
            Provenance::Refused(Refusal::UnknownToken)
        );
    }

    #[test]
    fn a_value_that_is_not_token_shaped_is_refused_as_malformed() {
        for junk in ["", "  ", "not-a-token", &"zz".repeat(32), &"aa".repeat(31)] {
            assert_eq!(
                classify("pane-worker", Some(junk), &fixture()),
                Provenance::Refused(Refusal::Malformed),
                "{junk:?} must be refused before any registry lookup"
            );
        }
    }

    /// The compatibility case, and the ONLY one the escape hatch moves: a
    /// message with no token at all, for a pane this daemon did issue one for.
    #[test]
    fn no_token_for_a_pane_that_has_one_is_missing() {
        assert_eq!(
            classify("pane-worker", None, &fixture()),
            Provenance::Refused(Refusal::Missing)
        );
    }

    /// A pane this daemon never spawned. Nothing to check against, so the
    /// message is passed through exactly as it was before this module existed —
    /// and, as `Provenance::Unattested` documents, it resolves to no target
    /// because the state that carries every authority is built by the same call
    /// that creates the record.
    #[test]
    fn an_unknown_pane_is_unattested_rather_than_refused() {
        assert_eq!(
            classify("pane-nobody-has", None, &fixture()),
            Provenance::Unattested
        );
    }

    #[test]
    fn enforce_admits_attested_and_unattested_and_refuses_the_rest() {
        let p = Policy::Enforce;
        assert!(
            admits(
                &Provenance::Attested {
                    agent_id: "a".into()
                },
                p
            )
            .is_ok()
        );
        assert!(admits(&Provenance::Unattested, p).is_ok());
        for refusal in [
            Refusal::Missing,
            Refusal::UnknownToken,
            Refusal::Malformed,
            Refusal::WrongPane { token_pane: None },
        ] {
            assert_eq!(
                admits(&Provenance::Refused(refusal.clone()), p),
                Err(refusal)
            );
        }
    }

    /// The escape hatch moves exactly one verdict and no other. This is the
    /// property that keeps `warn` a compatibility switch rather than an off
    /// switch: an old CLI omits the token entirely, so nothing it can send
    /// produces `UnknownToken` or `WrongPane`, and tolerating those would buy
    /// compatibility with nothing while re-opening the forgery.
    #[test]
    fn warn_only_moves_missing_and_nothing_else() {
        let p = Policy::WarnOnly;
        assert!(admits(&Provenance::Refused(Refusal::Missing), p).is_ok());
        for refusal in [
            Refusal::UnknownToken,
            Refusal::Malformed,
            Refusal::WrongPane {
                token_pane: Some("pane-worker".into()),
            },
        ] {
            assert_eq!(
                admits(&Provenance::Refused(refusal.clone()), p),
                Err(refusal),
                "the escape hatch must not tolerate a token-bearing forgery"
            );
        }
    }

    /// A knob that weakens a check must not be switchable by accident, so
    /// anything that is not exactly `warn` reads as the strict policy — a typo
    /// included.
    #[test]
    fn only_the_exact_word_warn_selects_the_permissive_policy() {
        assert_eq!(Policy::from_env_value(Some("warn")), Policy::WarnOnly);
        assert_eq!(Policy::from_env_value(Some("  WARN ")), Policy::WarnOnly);
        for raw in [
            None,
            Some(""),
            Some("enforce"),
            Some("warm"),
            Some("1"),
            Some("true"),
        ] {
            assert_eq!(
                Policy::from_env_value(raw),
                Policy::Enforce,
                "{raw:?} must not weaken the gate"
            );
        }
    }

    #[test]
    fn minted_tokens_are_well_formed_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let t = mint();
            assert!(is_well_formed(&t), "{t:?}");
            assert!(seen.insert(t), "mint must never repeat a token");
        }
    }

    #[test]
    fn token_comparison_is_length_and_content_exact() {
        let t = mint();
        assert!(tokens_match(&t, &t.clone()));
        assert!(!tokens_match(&t, &t[..TOKEN_LEN - 1]));
        let mut flipped: Vec<u8> = t.clone().into_bytes();
        flipped[TOKEN_LEN - 1] = if flipped[TOKEN_LEN - 1] == b'a' {
            b'b'
        } else {
            b'a'
        };
        assert!(!tokens_match(&t, std::str::from_utf8(&flipped).unwrap()));
    }

    /// A refusal must not become an enumeration oracle: the caller is told the
    /// message was refused and what to do about it, never which pane a token
    /// actually belongs to.
    #[test]
    fn a_refusal_message_never_names_the_token_s_real_pane() {
        let msg = Refusal::WrongPane {
            token_pane: Some("pane-orchestrator".to_string()),
        }
        .caller_message();
        assert!(
            !msg.contains("pane-orchestrator"),
            "the reply told the caller which pane the token belongs to: {msg}"
        );
    }

    /// See [`DOT_AGENT_DECK_PANE_CAPABILITY`] for why this is a real constraint:
    /// Codex's shell environment filter, when enabled, removes any variable whose
    /// name contains one of these, and a capability that never reaches the
    /// agent's shell makes every legitimate signal from that agent a refusal.
    #[test]
    fn the_capability_variable_survives_agents_secret_name_filters() {
        let upper = DOT_AGENT_DECK_PANE_CAPABILITY.to_ascii_uppercase();
        for filtered in ["KEY", "SECRET", "TOKEN"] {
            assert!(
                !upper.contains(filtered),
                "{DOT_AGENT_DECK_PANE_CAPABILITY} contains {filtered:?}, so an agent that scrubs \
                 secret-looking names from its shell tool would strip it"
            );
        }
    }

    #[test]
    fn token_from_env_reads_absent_and_blank_as_none() {
        // Pure-value half of `token_from_env`'s contract, without touching the
        // process environment (which is shared by every test in this binary).
        let normalise =
            |raw: Option<&str>| raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        assert_eq!(normalise(None), None);
        assert_eq!(normalise(Some("")), None);
        assert_eq!(normalise(Some("   ")), None);
        assert_eq!(normalise(Some(" abc ")), Some("abc".to_string()));
    }
}
