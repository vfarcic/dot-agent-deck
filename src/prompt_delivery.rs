//! Issue #424: the shared policy for **provisional** spawn-time prompt delivery.
//!
//! A write into a PTY is not a delivery. `SendResult::Applied` / `Queued` mean
//! only that the bytes reached the pane's writer (`src/pane.rs`); whether the
//! agent's TUI was in submit-CR-aware mode when the CR landed is unknowable from
//! the write's own return value. Against a Claude Code pane that is still
//! starting MCP servers the CR is swallowed and the pane comes up healthy, Idle,
//! and prompt-less — indistinguishable, to every pre-#424 delivery path, from a
//! prompt that was delivered and acted upon.
//!
//! The only honest evidence that a prompt was actually submitted is the agent
//! saying so, by reporting the submitted text back on an
//! [`EventType::Thinking`](crate::event::EventType::Thinking) event carrying
//! `user_prompt` (Claude/Codex `UserPromptSubmit`, OpenCode `session.prompt`).
//! So a write is **provisional** until such an event comes back for the same
//! pane carrying the same prompt text; until then the delivery keeps its prompt,
//! its identity and its retry armed, and re-submits under a bounded backoff.
//!
//! **Not every producer can do that**, which is why re-submission is gated on
//! [`agent_reports_submitted_prompt`] rather than on "some event carrying this
//! agent's id arrived". Reporting a LIFECYCLE and reporting SUBMITTED PROMPT
//! TEXT are separate capabilities: Pi's extension emits `agent-event` status
//! frames with the right pane and agent ids but hardcodes `user_prompt: None`
//! (`crate::main`'s `agent-event` subcommand), so treating any own-id event as
//! confirmation capability would arm a retry loop that is structurally unable
//! to ever confirm — retyping the prompt until the deadline. That is worse than
//! the bug being fixed, so the two capabilities are modelled apart.
//!
//! Three independent delivery implementations consume this module, which is why
//! the policy lives here rather than in any one of them:
//!
//! 1. [`crate::ui::deliver_orchestrator_prompt`] — an orchestration tab's
//!    spawn-time role prompt (TUI-owned).
//! 2. `crate::ui::process_pending_seed_prompts` — a `[[modes]]` `seed_prompt`
//!    (TUI-owned, PRD #127).
//! 3. [`crate::spawn::spawn`]'s delivery — `dispatch`, the scheduler and
//!    issue-dispatch (daemon-owned).
//!
//! Deliberately NOT merged into one implementation: that seam is already losing
//! prompts and a rewrite of it is a much larger, riskier change than the fix the
//! issue needs. See the coder's report on #424 for the follow-up proposal.

use chrono::{DateTime, Utc};

use crate::event::AgentType;

/// The byte length `crate::hook` truncates a reported `user_prompt` to before it
/// ever reaches [`crate::event::AgentEvent`].
///
/// **This is the silent-failure trap of the whole confirmation design.** A seed
/// prompt longer than this is reported back in truncated form, so comparing a
/// locally-known prompt to a reported one with plain `==` matches for short
/// prompts, never matches for long ones, and fails by *retrying until the
/// deadline abandons the prompt* — i.e. it looks exactly like the bug being
/// fixed. Always compare through [`prompt_submission_matches`], never directly.
pub const USER_PROMPT_MAX_LEN: usize = 200;

/// Hard cap on how long an automatic prompt (an orchestrator role prompt, a mode
/// seed, or a spawn-time dispatch prompt) is retried before it is abandoned with
/// visible feedback. Shared by all three delivery paths so "how long do we keep
/// trying" cannot drift between them.
pub const AUTOMATIC_PROMPT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// How many accumulated copies of one prompt the repetition shapes below build
/// candidates for before giving up.
///
/// This bounds the LOOP, not the accepted submissions. It is an exact bound only
/// while the reported text is complete: past [`USER_PROMPT_MAX_LEN`] every
/// repetition count collapses onto the same 200-byte prefix, so a report of 17,
/// 50 or 500 copies is indistinguishable from the 16 this builds — see
/// [`prompt_submission_matches`]'s residual section, which withdraws the claim
/// that accepted submissions are at most this many copies. The retry schedule
/// cannot produce more than single digits inside [`AUTOMATIC_PROMPT_DEADLINE`],
/// so the number is margin, not a tuning knob.
const MAX_REPEATED_SUBMISSION_COPIES: u32 = 16;

/// How many attempts of one logical delivery may write the PROMPT PAYLOAD before
/// later attempts fall back to the submit-only probe
/// ([`attempt_writes_payload`]).
///
/// Two, and each one is load-bearing:
///
/// * the FIRST write is the delivery itself;
/// * the SECOND is the one bounded REPLACEMENT payload. It has to exist because
///   a launcher genuinely CONSUMES the first one — `scheduler/dispatch/015`'s
///   bootstrap wrapper reads the bytes itself and the agent that starts behind it
///   never sees them — so a delivery that only ever probed submit after attempt 1
///   could never deliver in that case at all;
/// * every attempt after that probes BLINDLY. By then a payload may be sitting
///   unsubmitted in the agent's input box, and appending the whole prompt again
///   is what produced the observed `seedseed` accumulation;
/// * issue #666: a delivery that accumulates positive evidence its payload was
///   DESTROYED may make ONE further payload write beyond this cap
///   ([`AgentStartRearm`]). That is evidence, not patience — a third BLIND
///   rewrite is still refused forever, at every attempt number, and the total is
///   still hard-capped at three.
const MAX_PAYLOAD_SUBMISSIONS: u32 = 2;

/// Issue #666: whether this producer's native `SessionStart` is emitted BEFORE
/// it can accept its first prompt — i.e. whether the start is a STARTUP fact or
/// a TURN fact.
///
/// This is NOT [`agent_reports_submitted_prompt`] and must not be collapsed onto
/// it: Codex and OpenCode both report submitted prompt text (so they arm
/// retries) while their `SessionStart` arrives only once a prompt has been
/// accepted, so for them a post-write start is a CONSEQUENCE of our own delivery
/// rather than evidence the agent booted after it.
///
/// * Claude Code — native hook engine, `SessionStart` at session initialization,
///   before any turn. Measured throughout `scheduler/dispatch/014` and `/015`.
/// * Codex — codex-cli 0.145.0 posts its native `SessionStart` when the first
///   TURN starts (#243, measured: T+29.999 s start, T+30.004 s the injected
///   prompt's own `UserPromptSubmit`).
/// * OpenCode — `session.created` fires when the prompt is ACCEPTED, ~16 ms
///   after submission; a 35 s idle cold boot emits nothing (#146).
/// * Devin — ships a Claude-Code-compatible hooks engine, so it is LIKELY true,
///   and is answered `false` because nobody has measured it. An unverified
///   `true` is the unsafe direction: it would authorize re-writing a dispatch
///   task the agent has already accepted. Flip it when measured, in this one
///   place.
/// * Pi — never arms anything ([`ConfirmationCapability::CannotReport`]).
/// * [`AgentType::None`] — the `#[serde(other)]` forward-compat landing pad; an
///   unknown producer has proved nothing.
///
/// Exhaustive on purpose: a new agent type must answer this rather than inherit
/// a default. Issue #243 half 2 proposes exactly this predicate as an explicit
/// `AgentSpec` field; when that lands, this function's body becomes a registry
/// lookup and its call sites do not move.
pub fn agent_start_precedes_first_prompt(agent_type: &AgentType) -> bool {
    match agent_type {
        AgentType::ClaudeCode => true,
        AgentType::Codex
        | AgentType::OpenCode
        | AgentType::Devin
        | AgentType::Pi
        | AgentType::None => false,
    }
}

/// Issue #666: how long after the ARMING agent start the armed payload attempt
/// must wait before it may carry the payload.
///
/// The `crate::ui::SPAWN_TIME_READINESS_BUFFER` of the rearm, and for its
/// reason: a `SessionStart` means a session exists, not that the TUI interprets
/// `\r` as submit. Without it this fix would reproduce its own bug one level up
/// — an armed payload written the instant the start lands can be flushed by the
/// same raw-mode `tcsetattr(TCSAFLUSH)` it is recovering from.
///
/// Deliberately a SEPARATE constant from the 10 s-fallback buffer #529 is about:
/// that one guards a FIRST write against a gate timeout, this one guards an
/// ARMED rewrite against the start that armed it. They answer different
/// questions and must not be merged.
pub const REARM_READINESS_BUFFER: std::time::Duration = std::time::Duration::from_millis(500);

/// Issue #666: the ONE extra payload-carrying attempt a delivery may earn, and
/// all the state the arming decision reads.
///
/// # What it is for
///
/// The two blind payload writes ([`MAX_PAYLOAD_SUBMISSIONS`]) are written into a
/// pane whose agent may not exist yet. When a launcher consumes both — it reads
/// the bytes itself and only then `exec`s the real agent, or the agent's own
/// raw-mode transition flushes the tty — the delivery has spent every payload it
/// is allowed and every remaining attempt is a submit-only probe with nothing to
/// submit. That is issue #666's measured `alpha`: written at 23.5 s and 25.2 s,
/// probed at 26.2/28.3/32.5/…/70.9 s, abandoned at `attempts=8`.
///
/// A *blind* third rewrite stays refused forever — it is what produced the
/// `seedseed` accumulation of #424/#570. What this type adds is a third write
/// authorized by EVIDENCE: positive evidence that our bytes are NOT sitting in
/// the agent's input box.
///
/// # The six facts, all necessary
///
/// | # | fact | held by |
/// |---|---|---|
/// | S | **Standing** — before the write, something the deck can vouch for said WHICH AGENT TYPE occupies this pane | [`Self::believed_agent_type`], from the caller |
/// | U | **Unbound at the write** — the delivery had no bound hook generation when it last wrote its payload | [`Self::unbound_at_last_payload_write`], via [`Self::note_payload_write`] |
/// | G | **Genuine start** — a `SessionStart` NOT carrying wrapper-fork boot provenance | the caller, before it calls [`Self::observe_agent_start`] |
/// | I | **Identity** — same pane, exact non-optional `agent_id` match | the caller (already enforced by every observation site) |
/// | W | **After the write** — strictly after this delivery's ordering watermark / pre-write drain | the caller, plus [`Self::note_payload_write`] clearing stale observations |
/// | T | **The producer's start precedes its first prompt** | [`agent_start_precedes_first_prompt`], asked of the BELIEVED type |
///
/// G, I and W are the caller's because each delivery path observes producer
/// evidence through a different substrate (a broadcast receiver in the daemon, a
/// snapshot journal in the TUI) and each already enforces them at that point.
/// S, U and T are here, so the conjunction cannot drift between paths.
///
/// # Why S carries a TYPE and not a bool (issue #424 F4, the rule this restores)
///
/// **A declared `AgentType` may only ever WITHHOLD privilege, never grant it.**
/// The type on the arming `SessionStart` is an unauthenticated producer
/// assertion: the hook socket accepts raw `AgentEvent` JSON from any same-uid
/// process, so "I am Claude Code" is a claim, not a fact.
///
/// This type first shipped with S collapsed to a `bool` — "the deck vouched for
/// *something* here" — and T then asked of the DECLARED type alone. That is
/// exactly the grant #424 F4 forbids, and it was demonstrable rather than
/// theoretical: a pane the deck itself spawned as **Codex** (T false, because
/// codex-cli's start is a TURN fact) was armed by an event that merely *claimed*
/// to be Claude Code (T true). `scheduler/dispatch/016` cases **G** and **H** are
/// that hole, and they are the reciprocals of case **C**.
///
/// The fix is not "evaluate T against the trusted type instead" — that breaks C
/// in the other direction, arming a genuinely-Claude pane on a Codex-declaring
/// event. The rule is a CONJUNCTION: the declared type must MATCH what the deck
/// already believed about the pane, and that believed type must satisfy T. So a
/// declaration can only ever agree with a belief the deck already held, i.e.
/// withhold on disagreement, which is F4 restated.
///
/// | case | believed | declared | outcome |
/// |---|---|---|---|
/// | A | `ClaudeCode` (deck-spawn record) | `ClaudeCode` | match, T → **arm** |
/// | C | `ClaudeCode` (deck-spawn record) | `Codex` | mismatch → refuse |
/// | G | `Codex` (deck-spawn record) | `ClaudeCode` | mismatch → refuse |
/// | H | `OpenCode` (deck-spawn record) | `ClaudeCode` | mismatch → refuse |
/// | D | `ClaudeCode` (pre-write launcher declaration) | `ClaudeCode` | match, T → **arm** |
///
/// `Default`-disarmed on purpose: a value that was never given a believed type
/// can never arm, so forgetting to thread it is a no-op rather than a privilege.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentStartRearm {
    /// Fact S. WHICH AGENT TYPE the deck believed occupied this pane BEFORE the
    /// payload write, from a source the deck can vouch for. `None` — no belief —
    /// disarms permanently.
    believed_agent_type: Option<AgentType>,
    /// Fact U, as of the delivery's most recent payload write.
    unbound_at_last_payload_write: bool,
    /// Facts G ∧ I ∧ W ∧ T, observed since that write. `None` until then.
    observed_at: Option<std::time::Instant>,
    /// The one armed payload attempt has been spent.
    spent: bool,
}

impl AgentStartRearm {
    /// `believed` is fact S — a statement about WHAT OCCUPIES this pane, made
    /// before a byte of the prompt was written, from a source the deck can vouch
    /// for. `None` disarms permanently.
    ///
    /// On the daemon path it is
    /// [`crate::agent_pty::AgentPtyRegistry::pre_write_believed_agent_type`]:
    /// the deck's own frozen record of the agent type it exec'd, else the
    /// launcher declaration the pane made before the write.
    ///
    /// # The two halves of S are NOT equally strong
    ///
    /// * **The deck-spawn half is not producer-assertable.** It reads
    ///   [`crate::agent_pty::RunningAgent::spawn_agent_type`], the frozen
    ///   launch-shape identity the SPAWN SITE supplied; the learn-from-hook
    ///   upgrade (`AgentPtyRegistry::set_agent_type`) deliberately does not touch
    ///   it and no hook path writes it. A producer cannot manufacture this.
    /// * **The launcher-handoff half IS producer-assertable, and this is the
    ///   correction to a claim that was once made here.** It is set from nothing
    ///   but a received `wrapper_fork`-origin `SessionStart` whose pane and agent
    ///   ids match ([`crate::state::wait_for_session_start`]); nothing
    ///   authenticates its producer. What it establishes is that the declaration
    ///   was **not post hoc** — it was observed before our bytes went out — which
    ///   is an ORDERING property, not producer authentication. Requiring the
    ///   post-write declaration to match it therefore closes the MISMATCH hole
    ///   only; it does not make the belief unforgeable. Forging it is **#543**'s
    ///   ("Standing is not authentication"), and it buys strictly less than the
    ///   attach socket's `KIND_STREAM_IN` already grants the same uid: one write,
    ///   once, carrying the deck's own prompt text rather than bytes of the
    ///   forger's choosing.
    ///
    /// The launcher half cannot be dropped in the name of strength: it is the
    /// `devbox run claude …`-shaped population issue #666 is actually about,
    /// whose command does not resolve to an agent type and so has no deck-spawn
    /// record at all. `scheduler/dispatch/016` case D pins it.
    pub fn new(believed: Option<AgentType>) -> Self {
        Self {
            believed_agent_type: believed,
            unbound_at_last_payload_write: false,
            observed_at: None,
            spent: false,
        }
    }

    /// Called in the SAME arm that records an attempt, for every PAYLOAD write.
    ///
    /// Clears any earlier observation: a start seen BEFORE this write says
    /// nothing about THIS write's bytes. The evidence must postdate the bytes it
    /// is evidence about, which is fact W held across writes rather than only
    /// within one window.
    pub fn note_payload_write(&mut self, unbound_at_write: bool) {
        self.unbound_at_last_payload_write = unbound_at_write;
        self.observed_at = None;
    }

    /// Called wherever the path already observes post-write producer evidence,
    /// once per qualifying event.
    ///
    /// The caller has already established G ∧ I ∧ W; this adds T and the S ∧ U
    /// conjunction, and latches the FIRST qualifying instant — `at` is when the
    /// arming start was OBSERVED, not when the window carrying it expired, so
    /// [`REARM_READINESS_BUFFER`] is measured from the signal rather than from
    /// the caller's polling cadence.
    ///
    /// `declared` is the [`AgentType`] on the event. It is used ONLY to WITHHOLD
    /// (issue #424 F4), never as proof: it must AGREE with the belief the deck
    /// already held ([`Self::believed_agent_type`]), and T is then asked of that
    /// belief. A declaration that disagrees refuses; a declaration that agrees
    /// adds nothing the belief did not already carry. See the type-level table
    /// for the cases this decides.
    pub fn observe_agent_start(&mut self, at: std::time::Instant, declared: &AgentType) {
        if self.spent || self.observed_at.is_some() {
            return;
        }
        if !self.unbound_at_last_payload_write {
            return;
        }
        // Fact S: no pre-write belief, nothing to agree with, no arming. This is
        // the refusal `scheduler/dispatch/016` case B pins — a post-write claim
        // about a pane the deck cannot vouch for.
        let Some(believed) = self.believed_agent_type.as_ref() else {
            return;
        };
        // Issue #424 F4: withhold on disagreement. The declared type never
        // upgrades the belief, so a Codex-spawned pane is not armed by an event
        // claiming Claude (case G/H) and a Claude-spawned pane is not armed by
        // one claiming Codex (case C).
        if believed != declared {
            return;
        }
        // Fact T, asked of the BELIEVED type. Identical to asking it of
        // `declared` at this point — they are equal — and written this way
        // because the belief is the trusted half of the pair.
        if !agent_start_precedes_first_prompt(believed) {
            return;
        }
        self.observed_at = Some(at);
    }

    /// Whether the rearm may authorize a payload write at `now` — observed,
    /// unspent, and [`REARM_READINESS_BUFFER`] elapsed since the arming start.
    pub fn is_available(&self, now: std::time::Instant) -> bool {
        !self.spent
            && self
                .observed_at
                .is_some_and(|at| now.saturating_duration_since(at) >= REARM_READINESS_BUFFER)
    }

    /// Consume it. Called in the same arm that RECORDS the attempt, not where
    /// the decision is made: a retryable send outcome does not advance the
    /// attempt counter on the TUI paths, so it must not burn the rearm either.
    ///
    /// One-shot by two independent mechanisms — this flag, and the structural
    /// one: the arming event is a GENUINE `SessionStart`, and a second genuine
    /// start naming a different generation is already TERMINAL
    /// (`crate::state::latch_generation`), so a producer trying to replay the
    /// arming signal ends the delivery instead of re-arming it.
    pub fn spend(&mut self) {
        self.spent = true;
    }
}

/// Private, so the two public answers below cannot drift.
fn blind_payload(attempt: u32) -> bool {
    attempt <= MAX_PAYLOAD_SUBMISSIONS
}

/// Whether attempt `attempt` (1-based) of a delivery writes the prompt PAYLOAD,
/// or only probes submission.
///
/// Issue #424 (both reviewers, D5). A retry existed for one reason — the submit
/// CR may have been swallowed by a still-booting agent TUI — but it was
/// implemented as "type the whole prompt again", which is only the right recovery
/// when the first payload never landed. When the payload DID land and only its CR
/// was eaten, the payload is still sitting in the input box, so retyping appends a
/// second copy and the agent eventually submits `seedseed`: a corrupted task, and
/// one the confirmation matcher then rejects, which leaves the loop armed and
/// types a THIRD copy. Probing submit instead leaves the pending bytes alone and
/// simply asks the TUI to submit what it already holds.
///
/// The probe is not a weaker write: it goes through the same identity-guarded,
/// writer-serialized, deadline-bounded path as an ordinary attempt and classifies
/// a partial write the same way. It differs only in carrying an EMPTY payload, so
/// the encoder emits no bytes and the target receives just the delayed submit CR
/// ([`crate::pane_input::encode_pane_payload`],
/// `AgentPtyRegistry::write_and_submit_guarded`). No protocol change is involved.
pub fn attempt_writes_payload(
    attempt: u32,
    rearm: &AgentStartRearm,
    now: std::time::Instant,
) -> bool {
    blind_payload(attempt) || rearm.is_available(now)
}

/// Issue #666: whether this attempt's payload write is the one the REARM
/// authorized — i.e. whether it must CONSUME it. Never true for the two blind
/// attempts, so a delivery that confirms normally never spends anything.
///
/// Asked separately from [`attempt_writes_payload`] rather than inferred from it
/// because the two are consulted at different instants: the decision is made
/// before the write, the spend is recorded in the arm that records the attempt.
/// Both read the same `rearm` and `now`, so they cannot disagree.
pub fn attempt_spends_rearm(
    attempt: u32,
    rearm: &AgentStartRearm,
    now: std::time::Instant,
) -> bool {
    !blind_payload(attempt) && rearm.is_available(now)
}

/// Truncate `s` to at most `max` BYTES, appending `…` when anything was cut.
///
/// Slices on a char boundary at or below `max`: the naive `&s[..max]` panics
/// whenever the cut lands inside a multi-byte character, which in the hook
/// binary means a prompt containing any non-ASCII text past the limit kills the
/// hook process and emits no event at all.
pub fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Normalize a prompt for comparison using EXACTLY the PTY encoder's contract.
///
/// [`crate::pane_input::encode_pane_payload`] strips only trailing `\n`, `\r`,
/// space and tab before the bytes reach the PTY, so that — and only that — is
/// the difference between what we hand the encoder and what the agent submits
/// and reports back.
///
/// Reviewer finding B10: this used to be `str::trim`, which is both too wide and
/// wrong on the wrong end. Too wide because `trim` also removes trailing Unicode
/// whitespace the encoder PRESERVES (a NBSP, an ideographic space), so a
/// different submitted prompt differing only in such a character confirmed this
/// delivery; wrong on the wrong end because `trim` also strips LEADING
/// whitespace, which the encoder preserves and which can be meaningful in a
/// prompt (an indented code block, a leading blank line).
fn normalize_for_match(s: &str) -> &str {
    s.trim_end_matches(['\n', '\r', ' ', '\t'])
}

/// Whether a hook-reported `user_prompt` confirms submission of `expected` —
/// the prompt text we wrote into the pane.
///
/// Three shapes are accepted, each for a specific mechanical reason:
///
/// 1. **The text verbatim.** The ordinary case.
/// 2. **Its [`USER_PROMPT_MAX_LEN`]-truncated form.** Which one arrives depends
///    only on the prompt's length; see that constant.
/// 3. **N copies of it separated by newlines** (reviewer finding B5). When the
///    submit CR is swallowed by a still-booting agent TUI in the PRD #128 mode
///    where it becomes a *newline in the input box* rather than a no-op, the
///    payload stays in the box and the next retry's submission carries
///    `seed \n seed`. That is proof the seed was submitted — twice — so it must
///    finalize the delivery. Reading it as a mismatch instead left the loop
///    ARMED after the agent had already started acting, and a dispatch prompt
///    performs deployments, deletions and issue operations: running one a third
///    and fourth time is strictly worse than a visibly idle pane a human can
///    recover. This also removes an inconsistency that made the hazard
///    length-dependent — a seed longer than [`USER_PROMPT_MAX_LEN`] already
///    matched its doubled submission (both truncate to the same prefix) while a
///    short one did not, so short and long prompts behaved oppositely.
///
/// Both sides are normalized with [`normalize_for_match`], never `str::trim`.
///
/// # Accepted residual: the copy bound does not survive truncation (#526)
///
/// [`MAX_REPEATED_SUBMISSION_COPIES`] bounds shape 3 EXACTLY only while the
/// reported text is complete. Once the hook truncates at
/// [`USER_PROMPT_MAX_LEN`], every repetition count whose rendering exceeds that
/// limit collapses onto the SAME 200-byte prefix, so a submission of 17, 50 or
/// 500 copies is indistinguishable from the 16 this accepts — and so is an
/// arbitrary tail following a repeated expected prefix, since only the prefix
/// survives. The earlier claim that "accepted submissions are at most 16 copies
/// of the prompt" is therefore not enforceable and is withdrawn: what is
/// enforced is that the reported text BEGINS with a repetition of the expected
/// prompt. The bound is real for short prompts and is margin, not a security
/// property, for long ones.
///
/// This is the same collision class as the deferred full-prompt digest and is
/// folded into **#526**; a digest or delivery token is what would make the
/// comparison exact regardless of truncation. Not fixed here: the alternative
/// available at this seam — refusing every truncated report — would reject the
/// ordinary confirmation of any prompt over 200 bytes, i.e. reinstate issue
/// #424 for every long dispatch prompt.
pub fn prompt_submission_matches(expected: &str, reported: &str) -> bool {
    let expected = normalize_for_match(expected);
    let reported = normalize_for_match(reported);
    if expected.is_empty() {
        // Nothing was written, so nothing can be evidence about it. Guards the
        // repetition loop below against an infinite family of empty candidates.
        return false;
    }
    if reported == expected || reported == truncate_on_char_boundary(expected, USER_PROMPT_MAX_LEN)
    {
        return true;
    }
    // Shape 3. Built incrementally and bounded by the REPORTED length so a
    // multi-kilobyte dispatch prompt never allocates more than the hook could
    // possibly have reported.
    let mut candidate = String::from(expected);
    for _ in 2..=MAX_REPEATED_SUBMISSION_COPIES {
        candidate.push('\n');
        candidate.push_str(expected);
        if candidate.len() > reported.len() {
            // Every longer candidate is longer still, so only a TRUNCATED form
            // can match from here — and truncation is a fixed 200-byte prefix
            // of the same repeating text, identical for every larger copy
            // count. One check settles all of them.
            return reported == truncate_on_char_boundary(&candidate, USER_PROMPT_MAX_LEN);
        }
        if reported == candidate {
            return true;
        }
    }
    false
}

/// Whether a hook-reported `user_prompt` shows our prompt ACCUMULATED in the
/// agent's input box and submitted as N ≥ 2 copies run together with NO
/// separator — the `seedseed` shape observed in the field at the `waitUse` seam.
///
/// # Why this is not part of [`prompt_submission_matches`]
///
/// Both reviewers rejected widening the confirmation matcher to cover it, and
/// they are right: the matcher already has one repetition grammar whose bound
/// dissolves at the [`USER_PROMPT_MAX_LEN`] truncation boundary, and a second,
/// separator-free grammar makes the set of strings that count as PROOF OF
/// DELIVERY larger and harder to reason about in exactly the region where
/// truncation already erases the difference between candidates. The long-prompt
/// half of the tester's fixture demonstrates the hazard rather than a design to
/// copy: `long-bare` is accepted today only because the doubled text truncates
/// onto the ordinary long-prompt prefix.
///
/// So this is a strictly separate question, asked only after
/// [`prompt_submission_matches`] has already said no, and it authorizes strictly
/// less: it does not CONFIRM a delivery, it TERMINATES one. The distinction is
/// the whole point — the agent demonstrably submitted a turn whose text begins
/// with our prompt repeated, so it is already acting on something derived from
/// our task, and typing a third copy into it is the unsafe direction for a
/// dispatch prompt that deploys, deletes and publishes. Stopping is right;
/// claiming the delivery landed cleanly would not be.
///
/// This is a SAFETY NET, not the remedy. The remedy is [`attempt_writes_payload`]
/// — not producing the accumulation in the first place. The net exists because
/// prevention starts at attempt 3 and the doubling can already have happened by
/// then, and because a delivery whose first payload was consumed by a launcher
/// still writes one replacement.
pub fn prompt_submission_accumulated(expected: &str, reported: &str) -> bool {
    let expected = normalize_for_match(expected);
    let reported = normalize_for_match(reported);
    if expected.is_empty() {
        // Nothing was written, so nothing can be evidence about it — and an
        // empty `expected` would make the repetition loop below degenerate.
        return false;
    }
    // Built incrementally and bounded by the REPORTED length, exactly as the
    // newline-separated shape is, so a multi-kilobyte dispatch prompt never
    // allocates more than the hook could possibly have reported. Starts at TWO
    // copies: one copy is [`prompt_submission_matches`]'s business, and treating
    // it as accumulation here would terminate every ordinary delivery.
    let mut candidate = String::from(expected);
    for _ in 2..=MAX_REPEATED_SUBMISSION_COPIES {
        candidate.push_str(expected);
        if candidate.len() > reported.len() {
            // Every longer candidate is longer still, so only a TRUNCATED form
            // can match from here — and truncation is a fixed 200-byte prefix of
            // the same repeating text, identical for every larger copy count.
            return reported == truncate_on_char_boundary(&candidate, USER_PROMPT_MAX_LEN);
        }
        if reported == candidate {
            return true;
        }
    }
    false
}

/// Whether an agent of this type can report SUBMITTED PROMPT TEXT — the
/// capability the whole confirmation design rests on, as distinct from merely
/// reporting a lifecycle.
///
/// Reviewer finding B4. Claude Code and Devin post `UserPromptSubmit` through
/// the native hook engine, Codex's wrapper synthesizes the same shape, and
/// OpenCode forwards `session.prompt` — all four land as an event carrying
/// `user_prompt` (`crate::hook`). **Pi cannot**: its extension reaches the
/// daemon through the `agent-event` subcommand, which hardcodes
/// `user_prompt: None`, so a Pi pane emits perfectly well-formed status frames
/// carrying the right pane and agent ids and never a single submitted prompt.
/// Arming re-submission off those frames retypes the prompt until the deadline
/// into an agent that may already be working on it.
///
/// [`AgentType::None`] is BOTH "unrecognized binary" and the `#[serde(other)]`
/// forward-compat landing pad for an agent type this build has never heard of,
/// so it is answered `false`: an unknown producer has not proved the
/// capability, and the conservative answer only ever costs a retry we were not
/// entitled to make. The match is exhaustive on purpose — a new agent type must
/// answer this question rather than inherit a default.
pub fn agent_reports_submitted_prompt(agent_type: &AgentType) -> bool {
    match agent_type {
        AgentType::ClaudeCode | AgentType::OpenCode | AgentType::Codex | AgentType::Devin => true,
        AgentType::Pi | AgentType::None => false,
    }
}

/// What a pane is known to be able to do about confirming a delivery, as
/// distinct from what it has happened to do YET.
///
/// Reviewer finding B3. The old code collapsed this onto "did a readiness signal
/// arrive within 10 s", finalized the write the moment it had not, and threw the
/// prompt away. But "no readiness event yet" is not proof of a hookless shell —
/// it is equally a real Claude whose nested launcher, MCP startup, trust gate or
/// hook is slower than 10 s, and that pane can emit `SessionStart` at 10.1 s to
/// find its prompt already finalized and unretryable. That silently excluded the
/// `devbox run claude …` launcher class issue #424 §3 names as the most fragile
/// population from the entire fix. Capability is a property of the PRODUCER, so
/// it is read from the producer, and [`Unknown`](Self::Unknown) is a real state
/// the delivery waits in rather than a timeout verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationCapability {
    /// A producer that reports submitted prompt text owns this pane. An
    /// unconfirmed write may be re-submitted.
    Reports,
    /// A recognized producer that structurally cannot report one (Pi). The
    /// write is final: retrying could never be confirmed, only retyped.
    CannotReport,
    /// Nothing on this pane has identified itself yet — a bare shell, `cat`, a
    /// recorder stand-in, or an agent behind a launcher that has not signalled.
    /// The write is held PROVISIONAL but is NOT retyped, and this is
    /// re-evaluated every frame: the moment a real producer identifies itself,
    /// retries arm and the prompt is still recoverable.
    ///
    /// # Accepted behaviour change: hookless panes finalize at 60 s, not 10 s
    ///
    /// This state has a user-visible cost and it is deliberate. Before it, "no
    /// readiness signal after 10 s" was treated as a verdict — the prompt was
    /// finalized and discarded there — so a genuinely hookless pane settled
    /// quickly. Waiting for a producer instead means such a pane stays in its
    /// provisional state until [`AUTOMATIC_PROMPT_DEADLINE`], so a hookless
    /// orchestrator role remains `Waiting` roughly 50 s longer before being
    /// finalized `Working`, and a hookless mode seed stays pending for the same
    /// span. It affects real custom commands and hookless wrappers, not only
    /// synthetic `cat` panes.
    ///
    /// The reviewer explicitly declined to block on the trade, and it is kept:
    /// the 10 s verdict is what excluded the `devbox run claude …` launcher
    /// class — the population issue #424 §3 names as the most fragile — from the
    /// entire fix, because such a pane routinely identifies itself at 10.1 s and
    /// found its prompt already thrown away. Nothing is written twice while
    /// capability is unknown, so the cost is latency in a status label, not
    /// duplicated work.
    Unknown,
}

/// Resolve [`ConfirmationCapability`] from the agent types currently declared on
/// one pane's sessions. Any reporting producer wins; otherwise a recognized
/// non-reporting one settles it; otherwise the answer is not yet known.
pub fn pane_confirmation_capability<'a>(
    agent_types: impl Iterator<Item = &'a AgentType>,
) -> ConfirmationCapability {
    let mut capability = ConfirmationCapability::Unknown;
    for agent_type in agent_types {
        if agent_reports_submitted_prompt(agent_type) {
            return ConfirmationCapability::Reports;
        }
        if *agent_type != AgentType::None {
            capability = ConfirmationCapability::CannotReport;
        }
    }
    capability
}

/// Whether a reported submission at `event_ts` sorts AFTER the write this
/// delivery is trying to confirm, given the `watermark` captured from the same
/// event journal immediately before that write.
///
/// The name to keep in mind while reading this is ORDERING. An earlier revision
/// of these docs called the comparison "causal", and the residual section below
/// then spent a paragraph explaining that it is not — see #526. It rejects
/// everything that was already visible when we wrote, which is a strictly weaker
/// property than "was produced after our bytes".
///
/// Reviewer finding B2/#2. This used to be a wall-clock comparison against our
/// own write timestamp with a fixed five-second skew allowance, which was wrong
/// in three independent ways. It accepted an event from up to five seconds
/// BEFORE the write, so re-enqueueing a seed shortly after a previous
/// submission confirmed instantly off the *old* event with no clock skew
/// involved at all. And because a TUI's clock and the event producer's clock are
/// genuinely different clocks for a remote deck, a TUI more than five seconds
/// ahead rejected every real confirmation forever, while one running behind
/// widened the stale window by however far it lagged.
///
/// A watermark taken from the pane's own event journal has neither problem: it
/// is a value from the SAME clock that stamps the events being compared, so the
/// comparison is positional within one journal rather than chronometric across
/// two clocks. That removes the skew hazards; it does not make it causal — see
/// the residual below. `None` means the pane's
/// journal was empty when we wrote — there is no pre-existing history to
/// mistake for evidence, so anything that arrives afterwards is new.
///
/// # Accepted residual: this is ordering, not causality (#526)
///
/// The watermark rejects everything ALREADY VISIBLE when we wrote, which is the
/// concrete prior-event-in-journal case that was confirming deliveries off
/// history. It cannot reject an event that was PRODUCED before our write and
/// merely arrived after it: delivery order does not follow producer stamps (hook
/// sends arrive on separate connections handled by separate tasks), so such an
/// event can carry a timestamp above the captured maximum and confirm a write it
/// predates. Both reviews rate this Low/partial, and it is the direction that
/// costs a false CONFIRMATION rather than a false retry.
///
/// Closing it needs something this comparison does not have: a daemon RECEIVE
/// SEQUENCE (a cursor captured at admission, ordered by arrival rather than by
/// producer clock) or the deferred full-prompt digest / delivery token that
/// would identify the submission itself. Both are folded into **#526**. The
/// daemon-side twin of the gap is the `try_recv`-until-`Empty` drain in
/// `crate::spawn`, which has the same "queued at this instant" semantics and the
/// same limit.
pub fn submission_is_after_watermark(
    event_ts: DateTime<Utc>,
    watermark: Option<DateTime<Utc>>,
) -> bool {
    match watermark {
        Some(mark) => event_ts > mark,
        None => true,
    }
}

/// Backoff before re-submitting a written-but-UNCONFIRMED prompt: 0.5 s, 1 s,
/// 2 s, 4 s, 8 s, then capped at 15 s. `attempts` is the number of submissions
/// made so far (≥ 1).
///
/// Deliberately NOT `crate::ui::send_retry_delay`'s 2 s-capped schedule, which
/// exists for a target that is *refusing* delivery and may become live at any
/// moment, so it keeps catch-up latency small at the cost of frequent retries.
/// An unconfirmed write is the opposite case: the agent accepted the bytes and
/// is booting, the wait can legitimately run to tens of seconds (5-6 MCP servers
/// on the reported failure), and every retry is a *second copy of the prompt*
/// typed into whatever the pane is showing. Escalating to a 15 s cap keeps the
/// whole [`AUTOMATIC_PROMPT_DEADLINE`] window covered in single-digit attempts
/// instead of ~30.
pub fn unconfirmed_retry_delay(attempts: u32) -> std::time::Duration {
    const BASE_MS: u64 = 500;
    const CAP: std::time::Duration = std::time::Duration::from_secs(15);
    let shift = attempts.saturating_sub(1).min(8);
    std::time::Duration::from_millis(BASE_MS.saturating_mul(1u64 << shift)).min(CAP)
}

/// Mint a GLOBALLY-UNIQUE logical delivery id for an automatic prompt.
///
/// Combines a per-PROCESS nonce (two processes — and a pid reused across
/// restarts — never collide) with a global monotonic counter (two ids within one
/// process never collide), keyed by pane for log readability. The per-process
/// nonce matters because the daemon's dedup ledger outlives any one TUI: a
/// restarted TUI reusing a plain `seed-<pane>-<n>` could otherwise have a
/// genuinely-new prompt silently suppressed as a replay (PRD #20 finding #3).
pub fn mint_delivery_id(pane_id: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nonce = *NONCE.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut h);
        // Nanos since the epoch disambiguate a pid reused across restarts.
        if let Ok(dur) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            dur.as_nanos().hash(&mut h);
        }
        h.finish()
    });
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("send-{nonce:016x}-{pane_id}-{seq}")
}

/// The idempotency key put ON THE WIRE for attempt `attempt` (1-based) of the
/// logical delivery `delivery_id`.
///
/// Issue #424: the daemon's delivery ledger CACHES an `Applied`/`Queued`/
/// `Ambiguous` outcome per `delivery_id` and replays it for any later request
/// carrying that id — *without writing to the PTY again*
/// (`AgentPtyRegistry::admit_delivery`). That is exactly right for its original
/// purpose (a retry after a lost response must not double-submit) and exactly
/// wrong for an unconfirmed prompt, whose retry exists precisely to produce a
/// second physical submission: it would replay `Applied` forever and never touch
/// the pane again.
///
/// So the logical delivery keeps ONE stable identity — it is what the local
/// prompt/confirmation/retry state is keyed on, and what the logs correlate on —
/// while each ATTEMPT goes on the wire under its own derived id. The ledger
/// therefore sees a genuinely new delivery per attempt and admits it
/// (`Proceed`), while concurrent duplicates of the *same* attempt still collapse
/// onto one submission through the ledger's single-flight guard, and the
/// fingerprint check still refuses a reused id carrying a different payload.
///
/// The wire field is an opaque string with no charset or length validation
/// (`daemon_protocol::WriteAndSubmitExtras`), so this needs no protocol change.
pub fn attempt_delivery_id(delivery_id: &str, attempt: u32) -> String {
    format!("{delivery_id}#a{attempt}")
}

/// Info-level record that a prompt's bytes were written into `pane_id`.
///
/// Issue #424 §4: before this, the entire delivery path was silent, so a lost
/// seed left no trace anywhere and the failure could only be reconstructed from
/// process tables and file mtimes. "Unconditional" here means unconditional
/// *within* the delivery path — it is not gated behind extra verbosity — not
/// that it materializes a subscriber: `init_logging_from_env` installs one only
/// when `DOT_AGENT_DECK_LOG` is set, so with no log configured these calls are
/// no-ops rather than terminal noise.
pub fn log_prompt_written(path: &str, pane_id: &str, delivery_id: &str, attempt: u32) {
    tracing::info!(
        path,
        pane_id,
        delivery_id,
        attempt,
        "prompt written to pane; provisional until UserPromptSubmit confirms it"
    );
}

/// Info-level record that a written prompt is still UNCONFIRMED and is being
/// re-submitted.
pub fn log_prompt_unconfirmed(path: &str, pane_id: &str, delivery_id: &str, attempt: u32) {
    tracing::info!(
        path,
        pane_id,
        delivery_id,
        attempt,
        "prompt delivery unconfirmed; re-submitting"
    );
}

/// Info-level record that attempt `attempt` probed SUBMISSION only — no payload
/// bytes — because a payload may already be sitting unsubmitted in the agent's
/// input box. See [`attempt_writes_payload`].
pub fn log_prompt_probe_submitted(path: &str, pane_id: &str, delivery_id: &str, attempt: u32) {
    tracing::info!(
        path,
        pane_id,
        delivery_id,
        attempt,
        "prompt delivery unconfirmed; probing submit without rewriting the payload"
    );
}

/// Warn-level record that the agent reported submitting our prompt as REPEATED
/// COPIES run together with no separator — the accumulation
/// [`prompt_submission_accumulated`] recognizes.
///
/// Warn rather than info because it is not a clean delivery: the agent submitted
/// a turn whose text is our prompt doubled, so it is acting on something derived
/// from the task rather than on the task as written. The delivery is TERMINATED
/// on it — retyping a third copy into an agent already working is the unsafe
/// direction — and this line is what tells the two apart afterwards.
pub fn log_prompt_accumulated(path: &str, pane_id: &str, delivery_id: &str, attempts: u32) {
    tracing::warn!(
        path,
        pane_id,
        delivery_id,
        attempts,
        "agent reported the prompt submitted as repeated concatenated copies; \
         treating delivery as complete and stopping retries"
    );
}

/// Info-level record that the agent reported submitting the prompt we wrote —
/// the only evidence that turns a provisional write into a delivery.
pub fn log_prompt_confirmed(path: &str, pane_id: &str, delivery_id: &str, attempt: u32) {
    tracing::info!(
        path,
        pane_id,
        delivery_id,
        attempt,
        "prompt delivery confirmed by the agent's submitted prompt"
    );
}

/// Info-level record that this delivery has no confirmation channel at all, so
/// the write is final and no retry will be attempted. Emitted for a target that
/// cannot report submitted prompt text — no hook-event bus at all, or a
/// producer [`agent_reports_submitted_prompt`] answers `false` for. Such an
/// agent reports no submitted prompts, so retrying would only type the prompt
/// into it repeatedly and then abandon it.
///
/// Deliberately NOT emitted merely because a readiness signal has not arrived
/// yet (reviewer finding B3): "no `SessionStart` after 10 s" is equally the
/// state of a real Claude whose nested launcher, MCP startup or trust gate is
/// slower than that, and finalizing there re-created the exact silent loss
/// issue #424 is about for the most fragile population it names.
pub fn log_prompt_unconfirmable(path: &str, pane_id: &str, delivery_id: &str, reason: &str) {
    tracing::info!(
        path,
        pane_id,
        delivery_id,
        reason,
        "prompt written to pane; delivery cannot be confirmed by this agent, not retrying"
    );
}

/// Warn-level record that automatic re-submission has STOPPED for a reason that
/// is neither confirmation nor the deadline: the evidence needed to decide is
/// missing or the target we wrote to is gone.
///
/// The prompt is left as written — not retyped and not declared delivered —
/// because in every case that reaches here, writing again is the unsafe move:
///
/// * `lagged-event-stream` (reviewer finding B7): the daemon's observer
///   broadcast dropped frames, so the real `UserPromptSubmit` may have been
///   among them. Missing evidence is not permission to submit a second copy
///   into an agent that is already acting on the first.
/// * `event-stream-closed` (B8): nothing can ever report back now.
/// * `bound-session-ended` / `session-ended-while-unbound` / `generation-changed`
///   / `delivery-target-changed` / `agent-replaced` (B1/B2): the conversation we
///   wrote into is over or has been superseded, or the pane rebound to a
///   different agent. The prompt belongs to a target that no longer exists, and
///   the only thing a retry could do is inject it into a stranger's context.
///   Which of the five it is says WHERE the loss was detected — the bound
///   session's own end, an end seen before any generation was bound, a newer
///   generation announcing itself, the TUI's snapshot comparison, or the
///   registry identity guard — which is the difference between a `/clear`, a
///   wrapper restart and a pane-reuse race when reading a log after the fact.
/// * a `SendResult` name (`wrong session`, `ambiguous`, …): the target refused a
///   delivery that had already been written once, so it is gone; or the write
///   was partial and must never be blindly repeated.
pub fn log_prompt_stopped(path: &str, pane_id: &str, delivery_id: &str, reason: &str) {
    tracing::warn!(
        path,
        pane_id,
        delivery_id,
        reason,
        "prompt delivery stopped without confirmation; not re-submitting"
    );
}

/// Warn-level record that a prompt was never confirmed within
/// [`AUTOMATIC_PROMPT_DEADLINE`] and has been abandoned.
pub fn log_prompt_abandoned(path: &str, pane_id: &str, delivery_id: &str, attempts: u32) {
    tracing::warn!(
        path,
        pane_id,
        delivery_id,
        attempts,
        "prompt delivery unconfirmed at the deadline; abandoning"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn short_prompt_matches_verbatim() {
        assert!(prompt_submission_matches("hello there", "hello there"));
        assert!(!prompt_submission_matches("hello there", "hello world"));
    }

    #[test]
    fn trailing_whitespace_is_not_a_mismatch() {
        // `encode_pane_payload` strips trailing whitespace before the bytes
        // reach the PTY, so the agent reports the trimmed form.
        assert!(prompt_submission_matches("do the thing\n", "do the thing"));
    }

    /// Reviewer finding B10: normalization must be the ENCODER's contract, not
    /// `str::trim`. Leading whitespace is preserved by the encoder and so is
    /// meaningful; trailing Unicode whitespace is preserved too, so it cannot
    /// be normalized away into a different prompt's report.
    #[test]
    fn normalization_matches_the_encoder_contract() {
        assert!(!prompt_submission_matches("  indented", "indented"));
        assert!(prompt_submission_matches("  indented", "  indented"));
        // U+00A0 NO-BREAK SPACE survives `encode_pane_payload`, so two prompts
        // differing only by one are DIFFERENT prompts.
        assert!(!prompt_submission_matches("do it\u{a0}", "do it"));
        // ...while the four bytes the encoder does strip are still equivalent.
        assert!(prompt_submission_matches("do it \t\r\n", "do it"));
    }

    /// Reviewer finding B5: the CR-swallow shape. The payload stayed in the
    /// input box, the retry submitted, and the hook reports `seed \n seed`.
    /// That is the seed submitted — twice — so it must finalize the delivery
    /// rather than leave the retry armed against an agent already working.
    #[test]
    fn repeated_copies_of_the_seed_count_as_delivered() {
        let seed = "Read .dot-agent-deck/worker-task-coder.md and begin";
        assert!(prompt_submission_matches(seed, &format!("{seed}\n{seed}")));
        assert!(prompt_submission_matches(
            seed,
            &format!("{seed}\n{seed}\n{seed}")
        ));
        // Short and long prompts must behave the SAME way here; before this
        // they behaved oppositely, because a long prompt's doubled submission
        // truncates to the same prefix while a short one's does not.
        let long = "y".repeat(USER_PROMPT_MAX_LEN + 40);
        let doubled = format!("{long}\n{long}");
        assert!(prompt_submission_matches(
            &long,
            &truncate_on_char_boundary(&doubled, USER_PROMPT_MAX_LEN)
        ));
        // A prompt whose doubled form crosses the truncation limit only from
        // the second copy on — the case neither the verbatim nor the
        // single-truncation branch can reach.
        let medium = "m".repeat(USER_PROMPT_MAX_LEN - 20);
        let medium_doubled = format!("{medium}\n{medium}");
        assert!(prompt_submission_matches(
            &medium,
            &truncate_on_char_boundary(&medium_doubled, USER_PROMPT_MAX_LEN)
        ));
        // Repetition must not become a wildcard: a different prompt appended
        // after ours is NOT proof that only ours was submitted.
        assert!(!prompt_submission_matches(
            seed,
            &format!("{seed}\nnow delete the production database")
        ));
        assert!(!prompt_submission_matches(seed, ""));
        assert!(!prompt_submission_matches("", "anything"));
    }

    /// D5: the separator-free `seedseed` shape observed in the field is
    /// recognized as ACCUMULATION — terminal evidence — but stays out of the
    /// confirmation matcher, which both reviewers refused to widen.
    #[test]
    fn bare_concatenation_is_accumulation_and_not_confirmation() {
        let seed = "Use Bash to verify seed-confirm-alpha-7f31.txt exists then print it and wait";
        let doubled = format!("{seed}{seed}");
        assert!(
            !prompt_submission_matches(seed, &doubled),
            "the confirmation matcher must NOT learn a second repetition grammar"
        );
        assert!(prompt_submission_accumulated(seed, &doubled));
        assert!(prompt_submission_accumulated(
            seed,
            &format!("{seed}{seed}{seed}")
        ));
        // The hook-truncated form of the same accumulation, which is all a
        // report of a prompt this long can carry.
        assert!(prompt_submission_accumulated(
            seed,
            &truncate_on_char_boundary(&doubled, USER_PROMPT_MAX_LEN)
        ));

        // One copy is the ordinary delivery, never accumulation — otherwise
        // every confirmed prompt would terminate through this path instead.
        assert!(!prompt_submission_accumulated(seed, seed));
        assert!(!prompt_submission_accumulated(
            seed,
            &truncate_on_char_boundary(seed, USER_PROMPT_MAX_LEN)
        ));
        // Accumulation must not become a wildcard either: a different task
        // appended after ours is not our prompt doubled.
        assert!(!prompt_submission_accumulated(
            seed,
            &format!("{seed}now delete the production database")
        ));
        assert!(!prompt_submission_accumulated(seed, ""));
        assert!(!prompt_submission_accumulated("", "anything"));
        // The newline-separated shape belongs to the matcher, not here.
        assert!(!prompt_submission_accumulated(
            seed,
            &format!("{seed}\n{seed}")
        ));
    }

    /// D5: the retry schedule writes the payload twice and probes thereafter.
    ///
    /// Issue #666 restates it against an explicitly DISARMED rearm, which is the
    /// state every delivery is in unless it earns otherwise. This is the BLIND
    /// refusal contract — the thing that stops the `seedseed` accumulation of
    /// #424/#570 — and nothing in #666 softens it: without evidence, attempt 3
    /// and every attempt after it probes, forever.
    #[test]
    fn only_the_first_two_attempts_write_the_payload() {
        let now = Instant::now();
        let disarmed = AgentStartRearm::default();
        assert!(
            attempt_writes_payload(1, &disarmed, now),
            "attempt 1 IS the delivery's payload"
        );
        assert!(
            attempt_writes_payload(2, &disarmed, now),
            "one bounded replacement payload, because /015's launcher consumes the first"
        );
        for attempt in 3..=12 {
            assert!(
                !attempt_writes_payload(attempt, &disarmed, now),
                "attempt {attempt} must probe submit rather than append another copy"
            );
            // ...and a disarmed delivery has nothing to spend at any attempt,
            // so the probe cannot be mistaken for a consumed rearm.
            assert!(
                !attempt_spends_rearm(attempt, &disarmed, now),
                "attempt {attempt} of a disarmed delivery consumes nothing"
            );
        }
    }

    /// Build the fully-armed rearm of issue #666: a pre-write BELIEF about what
    /// occupies the pane, unbound at the last payload write, and a qualifying
    /// genuine start observed at `t0` that AGREES with that belief.
    fn armed_at(t0: Instant) -> AgentStartRearm {
        let mut rearm = AgentStartRearm::new(Some(AgentType::ClaudeCode));
        rearm.note_payload_write(true);
        rearm.observe_agent_start(t0, &AgentType::ClaudeCode);
        rearm
    }

    /// Issue #666: the one armed payload attempt, and the buffer in front of it.
    ///
    /// The buffer exists for the same reason `SPAWN_TIME_READINESS_BUFFER` does —
    /// a `SessionStart` means a session exists, not that the TUI interprets `\r`
    /// as submit — and without it the fix reproduces its own bug one level up: a
    /// payload written the instant the start lands is flushed by the same
    /// `tcsetattr(TCSAFLUSH)` it is recovering from. Being early costs nothing:
    /// the attempt falls back to the ordinary probe and the arming is RETAINED.
    #[test]
    fn an_armed_delivery_writes_one_more_payload_once_the_buffer_elapses() {
        let t0 = Instant::now();
        let rearm = armed_at(t0);

        assert!(
            !attempt_writes_payload(3, &rearm, t0),
            "a payload written the instant the arming start lands can be flushed by the \
             raw-mode transition that armed it"
        );
        assert!(
            !attempt_writes_payload(3, &rearm, t0 + Duration::from_millis(499)),
            "REARM_READINESS_BUFFER is 500 ms and it is a floor, not a target"
        );
        assert!(
            !attempt_spends_rearm(3, &rearm, t0 + Duration::from_millis(499)),
            "an attempt that fell back to the probe must not consume the arming"
        );

        assert!(
            attempt_writes_payload(3, &rearm, t0 + REARM_READINESS_BUFFER),
            "issue #666 alpha: attempts 1 and 2 were destroyed by the launcher and the \
             agent came up afterwards, so attempt 3 carries the payload into a live box"
        );
        assert!(
            attempt_spends_rearm(3, &rearm, t0 + REARM_READINESS_BUFFER),
            "the armed attempt is the one that must consume the arming"
        );
        assert!(
            attempt_writes_payload(7, &rearm, t0 + Duration::from_secs(30)),
            "the arming is not tied to an attempt NUMBER — a delivery armed late \
             spends it at whatever attempt comes next"
        );
    }

    /// Issue #666: ONE payload write per logical delivery may ever be authorized
    /// by the rearm, so the total is hard-capped at three.
    ///
    /// The explicit flag is only half of it — the other half is structural, and
    /// stronger: the arming event is a GENUINE `SessionStart`, and a second one
    /// naming a different generation is already terminal
    /// (`crate::state::latch_generation`), so a producer replaying the arming
    /// signal ends the delivery rather than re-arming it. `scheduler/dispatch/016`
    /// case F pins that half.
    #[test]
    fn the_armed_payload_attempt_is_one_shot() {
        let t0 = Instant::now();
        let mut rearm = armed_at(t0);
        let armed_now = t0 + REARM_READINESS_BUFFER;
        assert!(attempt_spends_rearm(3, &rearm, armed_now));

        rearm.spend();

        for attempt in 3..=12 {
            for extra_secs in [0u64, 1, 30, 3600] {
                let now = armed_now + Duration::from_secs(extra_secs);
                assert!(
                    !attempt_writes_payload(attempt, &rearm, now),
                    "attempt {attempt} at +{extra_secs}s must probe: the rearm is spent"
                );
                assert!(!attempt_spends_rearm(attempt, &rearm, now));
            }
        }

        // A spent rearm cannot be re-armed by a later qualifying start either.
        rearm.observe_agent_start(armed_now, &AgentType::ClaudeCode);
        assert!(!attempt_writes_payload(
            3,
            &rearm,
            armed_now + REARM_READINESS_BUFFER
        ));
    }

    /// Issue #666: the two BLIND payload attempts write for their own reasons
    /// (the delivery itself, and the one bounded replacement), so neither may
    /// burn the one armed write a delivery might still need.
    #[test]
    fn the_two_blind_payload_attempts_never_consume_the_rearm() {
        let t0 = Instant::now();
        let rearm = armed_at(t0);
        let now = t0 + REARM_READINESS_BUFFER;
        for attempt in [1, 2] {
            assert!(attempt_writes_payload(attempt, &rearm, now));
            assert!(
                !attempt_spends_rearm(attempt, &rearm, now),
                "attempt {attempt} writes its payload blindly and owes the rearm nothing"
            );
        }
    }

    /// Issue #666: all six arming facts are necessary — no five of them suffice.
    ///
    /// Five are exercised here by flipping exactly one input away from the armed
    /// case. The sixth, **G** (the start must not carry wrapper-fork boot
    /// provenance), is a CALLER-side filter by construction: the rearm is handed
    /// an instant and a declared type, never an event, because each delivery path
    /// observes producer evidence through a different substrate. It is pinned
    /// where that filter lives — `crate::spawn`'s
    /// `a_wrapper_fork_start_is_not_arming_evidence_for_the_rearm`.
    #[test]
    fn every_one_of_the_rearm_facts_is_necessary() {
        let t0 = Instant::now();
        let armed_now = t0 + REARM_READINESS_BUFFER;

        // S — the deck believed nothing about this pane before the write. An
        // unauthenticated post-write producer claim on a pane the deck cannot
        // vouch for buys nothing, which is the refusal `scheduler/dispatch/016`
        // case B pins.
        let mut no_standing = AgentStartRearm::new(None);
        no_standing.note_payload_write(true);
        no_standing.observe_agent_start(t0, &AgentType::ClaudeCode);
        assert!(
            !attempt_writes_payload(3, &no_standing, armed_now),
            "S: a pane the deck cannot vouch for may not authorize a payload rewrite"
        );

        // U — a conversation had already announced itself when we wrote, so the
        // agent was up and our bytes are sitting in its box. This is the healthy
        // fast path and it must never arm.
        let mut bound_at_write = AgentStartRearm::new(Some(AgentType::ClaudeCode));
        bound_at_write.note_payload_write(false);
        bound_at_write.observe_agent_start(t0, &AgentType::ClaudeCode);
        assert!(
            !attempt_writes_payload(3, &bound_at_write, armed_now),
            "U: a delivery whose agent had already announced a conversation when we wrote \
             would be APPENDING, not recovering"
        );

        // T — the producer's native start does not precede its first prompt, so a
        // post-write start is a consequence of the prompt we already delivered.
        // The belief AGREES with the declaration in every one of these, so the
        // mismatch rule is out of the way and T alone decides: a pane the deck
        // genuinely spawned as Codex, announcing itself as Codex, still refuses.
        for declared in [
            AgentType::Codex,
            AgentType::OpenCode,
            AgentType::Devin,
            AgentType::Pi,
            AgentType::None,
        ] {
            let mut wrong_producer = AgentStartRearm::new(Some(declared.clone()));
            wrong_producer.note_payload_write(true);
            wrong_producer.observe_agent_start(t0, &declared);
            assert!(
                !attempt_writes_payload(3, &wrong_producer, armed_now),
                "T: {declared:?}'s post-write start is caused by our own delivery, so arming \
                 on it would write a THIRD copy of a task the agent has already accepted"
            );
        }

        // W/I/G, the observation itself — nothing qualifying was ever seen.
        let mut never_observed = AgentStartRearm::new(Some(AgentType::ClaudeCode));
        never_observed.note_payload_write(true);
        assert!(
            !attempt_writes_payload(3, &never_observed, armed_now),
            "standing and an unbound write are pre-write facts; without post-write \
             evidence they authorize nothing"
        );

        // ...and the full conjunction does arm, so the five negatives above are
        // each the ONLY difference.
        assert!(attempt_writes_payload(3, &armed_at(t0), armed_now));
    }

    /// Issue #666 / #424 F4: a declared `AgentType` may only ever WITHHOLD
    /// privilege, never GRANT it.
    ///
    /// The rearm first shipped with fact S collapsed to a `bool` and T asked of
    /// the DECLARED type alone, so a pane the deck itself exec'd as Codex was
    /// armed by an event that merely CLAIMED to be Claude Code — the declaration
    /// granting a privilege the pane's real occupant does not have.
    /// `scheduler/dispatch/016` cases G and H are that hole end to end; this is
    /// the same contract as pure data, including the two directions of it that
    /// have to hold at once.
    ///
    /// Note what is NOT the fix: evaluating T against the trusted type alone.
    /// That breaks the reciprocal (case C) — a genuinely-Claude pane would arm on
    /// a Codex-declaring event, because the trusted half says Claude. Both
    /// directions are refusals, so the rule is the CONJUNCTION: agree, then
    /// satisfy T.
    #[test]
    fn a_declared_type_may_only_withhold_the_rearm_never_grant_it() {
        let t0 = Instant::now();
        let armed_now = t0 + REARM_READINESS_BUFFER;
        let observed = |believed: Option<AgentType>, declared: &AgentType| {
            let mut rearm = AgentStartRearm::new(believed);
            rearm.note_payload_write(true);
            rearm.observe_agent_start(t0, declared);
            attempt_writes_payload(3, &rearm, armed_now)
        };

        // G/H: the trusted belief fails T, and a declaration that passes it must
        // not upgrade the pane. This is the direction that shipped broken.
        for believed in [AgentType::Codex, AgentType::OpenCode] {
            assert!(
                !observed(Some(believed.clone()), &AgentType::ClaudeCode),
                "a pane the deck spawned as {believed:?} may not be armed by an event                  claiming to be Claude Code: the declared type would be GRANTING privilege"
            );
        }

        // C: the reciprocal, and the reason the fix is not "trust the spawn type
        // and ignore the declaration".
        assert!(
            !observed(Some(AgentType::ClaudeCode), &AgentType::Codex),
            "a Claude Code pane announcing itself as Codex is a disagreement, and a              disagreement refuses in BOTH directions"
        );

        // A/D: agreement, on a type that satisfies T, is the only thing that arms.
        assert!(
            observed(Some(AgentType::ClaudeCode), &AgentType::ClaudeCode),
            "the belief and the declaration agree on a producer whose start precedes              its first prompt — the one shape #666 recovers"
        );
    }

    /// Issue #666: the evidence must postdate the bytes it is evidence about.
    ///
    /// A start seen before payload write *N* says nothing about write *N*'s
    /// bytes, so every payload write clears the observation. Without this, the
    /// replacement payload (attempt 2) could inherit an arming produced before
    /// it and immediately authorize a third — a blind third rewrite wearing an
    /// evidence badge.
    #[test]
    fn a_start_observed_before_the_newest_payload_write_arms_nothing() {
        let t0 = Instant::now();
        let mut rearm = AgentStartRearm::new(Some(AgentType::ClaudeCode));
        rearm.note_payload_write(true);
        rearm.observe_agent_start(t0, &AgentType::ClaudeCode);
        assert!(attempt_writes_payload(
            3,
            &rearm,
            t0 + REARM_READINESS_BUFFER
        ));

        // The replacement payload goes out. Whatever that start proved, it did
        // not prove anything about THESE bytes.
        rearm.note_payload_write(true);
        for extra_secs in [0u64, 1, 30, 3600] {
            assert!(
                !attempt_writes_payload(3, &rearm, t0 + Duration::from_secs(extra_secs)),
                "an arming observed before the newest payload write must not survive it"
            );
        }
    }

    /// Issue #666, fact T. This is NOT `agent_reports_submitted_prompt` and must
    /// never be collapsed onto it: Codex and OpenCode both report submitted
    /// prompt text, so they arm the ordinary retry schedule, while their
    /// `SessionStart` arrives only once a prompt has been accepted.
    ///
    /// The measurements are in the assert messages on purpose — a future
    /// contributor flipping one of these has to argue with the evidence rather
    /// than with a taste.
    #[test]
    fn only_producers_whose_start_precedes_their_first_prompt_may_arm_a_rewrite() {
        assert!(
            agent_start_precedes_first_prompt(&AgentType::ClaudeCode),
            "Claude Code's native hook engine posts SessionStart at session \
             initialization, before any turn (measured throughout \
             scheduler/dispatch/014 and /015)"
        );
        assert!(
            !agent_start_precedes_first_prompt(&AgentType::Codex),
            "#243, measured on codex-cli 0.145.0: the native SessionStart fires when the \
             first TURN starts — T+29.999 s start against T+30.004 s for the injected \
             prompt's own UserPromptSubmit — so a post-write start is CAUSED by our delivery"
        );
        assert!(
            !agent_start_precedes_first_prompt(&AgentType::OpenCode),
            "#146, measured on 1.18.16: session.created fires when the prompt is ACCEPTED, \
             ~16 ms after submission, and a 35 s idle cold boot emits nothing at all"
        );
        assert!(
            !agent_start_precedes_first_prompt(&AgentType::Devin),
            "Devin ships a Claude-Code-compatible hooks engine so this is LIKELY true, and \
             is answered false because nobody has measured it — an unverified true would \
             authorize re-writing a dispatch task the agent has already accepted"
        );
        assert!(
            !agent_start_precedes_first_prompt(&AgentType::Pi),
            "Pi is ConfirmationCapability::CannotReport and arms nothing, ever"
        );
        assert!(
            !agent_start_precedes_first_prompt(&AgentType::None),
            "the #[serde(other)] forward-compat landing pad: an unknown producer has \
             proved nothing"
        );
    }

    /// Reviewer finding B4: reporting a lifecycle and reporting submitted
    /// prompt text are different capabilities, and Pi is the shipped
    /// counterexample that proves it.
    #[test]
    fn only_producers_that_can_report_a_prompt_arm_retries() {
        assert!(agent_reports_submitted_prompt(&AgentType::ClaudeCode));
        assert!(agent_reports_submitted_prompt(&AgentType::Codex));
        assert!(agent_reports_submitted_prompt(&AgentType::OpenCode));
        assert!(agent_reports_submitted_prompt(&AgentType::Devin));
        assert!(
            !agent_reports_submitted_prompt(&AgentType::Pi),
            "Pi's agent-event frames hardcode user_prompt: None, so a Pi \
             delivery can never be confirmed and must never arm a retry"
        );
        assert!(
            !agent_reports_submitted_prompt(&AgentType::None),
            "an unrecognized or future producer has not proved the capability"
        );
    }

    /// The trap from the tester's finding #2: a prompt longer than the hook's
    /// truncation limit is reported back truncated, so exact equality never
    /// matches and the delivery would retry until the deadline abandoned it.
    #[test]
    fn long_prompt_matches_its_truncated_report() {
        let long = "x".repeat(USER_PROMPT_MAX_LEN + 40);
        let reported = truncate_on_char_boundary(&long, USER_PROMPT_MAX_LEN);
        assert_ne!(reported, long, "the fixture must actually be truncated");
        assert!(prompt_submission_matches(&long, &reported));
        assert!(!prompt_submission_matches(&long, &"y".repeat(50)));
    }

    /// Truncation must never split a multi-byte character — the naive
    /// `&s[..max]` panics there, which in the hook binary means no event at all.
    #[test]
    fn truncation_never_splits_a_multibyte_char() {
        // 'é' is 2 bytes, so byte 200 lands mid-character.
        let s = format!("{}é{}", "a".repeat(199), "b".repeat(60));
        let cut = truncate_on_char_boundary(&s, USER_PROMPT_MAX_LEN);
        assert_eq!(cut, format!("{}…", "a".repeat(199)));
        assert!(prompt_submission_matches(&s, &cut));
    }

    #[test]
    fn unconfirmed_backoff_escalates_then_caps() {
        assert_eq!(
            unconfirmed_retry_delay(1),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(
            unconfirmed_retry_delay(2),
            std::time::Duration::from_secs(1)
        );
        assert_eq!(
            unconfirmed_retry_delay(3),
            std::time::Duration::from_secs(2)
        );
        assert_eq!(
            unconfirmed_retry_delay(5),
            std::time::Duration::from_secs(8)
        );
        assert_eq!(
            unconfirmed_retry_delay(9),
            std::time::Duration::from_secs(15)
        );
        // The whole deadline window is covered in single-digit attempts.
        let mut total = std::time::Duration::ZERO;
        let mut attempts = 0u32;
        while total < AUTOMATIC_PROMPT_DEADLINE {
            attempts += 1;
            total += unconfirmed_retry_delay(attempts);
        }
        assert!(
            attempts <= 9,
            "{attempts} submissions to cover the deadline"
        );
    }

    #[test]
    fn attempt_ids_are_distinct_per_attempt_and_share_their_logical_id() {
        let logical = mint_delivery_id("pane-7");
        let first = attempt_delivery_id(&logical, 1);
        let second = attempt_delivery_id(&logical, 2);
        assert_ne!(first, second, "each attempt must be a new ledger identity");
        assert!(first.starts_with(&logical) && second.starts_with(&logical));
    }

    #[test]
    fn minted_delivery_ids_are_unique() {
        let a = mint_delivery_id("pane-1");
        let b = mint_delivery_id("pane-1");
        assert_ne!(a, b);
    }

    /// Reviewer finding B2/#2: the watermark is CAUSAL, so an event that was
    /// already in the pane's journal when we wrote can never confirm the write
    /// — including the identical seed submitted a second before it.
    #[test]
    fn only_events_after_the_watermark_can_confirm() {
        let mark = Utc::now();
        assert!(!submission_is_after_watermark(
            mark - chrono::TimeDelta::seconds(1),
            Some(mark)
        ));
        assert!(
            !submission_is_after_watermark(mark, Some(mark)),
            "the watermark event IS the pre-existing history; it is not evidence"
        );
        assert!(submission_is_after_watermark(
            mark + chrono::TimeDelta::milliseconds(1),
            Some(mark)
        ));
        assert!(
            submission_is_after_watermark(mark - chrono::TimeDelta::hours(1), None),
            "an empty journal has no history to mistake for evidence"
        );
    }
}
