//! Fast-tier, pure-data coverage for issue #243's two readiness facts — the
//! per-agent one the registry declares, and the per-event one the wire carries.
//!
//! `AgentSpec::pre_prompt_readiness` answers one question — *what does a
//! FRESHLY STARTED instance of this agent announce BEFORE it is given a
//! prompt?* — and the delegate and scheduler readiness gates both read it to
//! decide whether waiting for a `SessionStart` is a timeout or pure dead time.
//! Before #243 the gates asked `hook_install.is_some()` instead, which answers
//! "does this agent have native hooks" and happened to be right for Claude and
//! wrong for both Codex and OpenCode: one mis-predicate, two victims, 31.2 /
//! 31.2 / 31.7 / 31.7 / 32.3 s of measured dead wait on production Codex
//! delegates.
//!
//! The behavioural end of that fix is covered by
//! `orchestration/delegate/029`, `/030` and `codex/wrap/006`. What is covered
//! HERE is the data those tests happen to exercise, per agent and
//! exhaustively — because behavioural coverage is necessarily partial: `/030`
//! pins OpenCode, `/007` pins Codex, `/008` and `/011` pin the neutral
//! placeholder, and nothing at all pins Claude Code or Devin, whose values are
//! precisely the ones a careless refactor of the old `hook_install` predicate
//! would get wrong.
//!
//! `agent_readiness_003` covers the OTHER fact, added by #243's review: the
//! `session_start_origin` marker a wrapper stamps on the `SessionStart` it emits.
//! There are three values and three predicates over them, they are priced
//! differently at the delegate's buffer seam (only `wrapper_interface_ready`
//! skips it), and the behavioural tests that exercise them each reach exactly one
//! value — so the table itself is pinned here, where an accidental widening is
//! visible instead of silently repricing the seam.

use std::collections::HashMap;

use dot_agent_deck::agent_registry::{self, PrePromptReadiness};
use dot_agent_deck::event::{
    AgentEvent, AgentType, EventType, SESSION_START_ORIGIN_METADATA_KEY,
    WRAPPER_FORK_SESSION_START_ORIGIN, WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN,
    WRAPPER_INTERFACE_SETTLED_SESSION_START_ORIGIN,
};
use spec::spec;

/// A `SessionStart` carrying `origin` under the origin metadata key, or none at
/// all when `origin` is `None` — the shape every non-wrapper producer posts.
fn session_start_with_origin(origin: Option<&str>) -> AgentEvent {
    let mut metadata = HashMap::new();
    if let Some(origin) = origin {
        metadata.insert(
            SESSION_START_ORIGIN_METADATA_KEY.to_string(),
            origin.to_string(),
        );
    }
    AgentEvent {
        session_id: "session-origin-table".to_string(),
        agent_type: AgentType::Codex,
        event_type: EventType::SessionStart,
        tool_name: None,
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata,
        pane_id: Some("pane-origin-table".to_string()),
        agent_id: Some("agent-origin-table".to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
    }
}

/// Every `AgentType` the registry resolves, including the neutral "no agent"
/// placeholder. `agent_registry::ALL` deliberately omits that placeholder (it
/// is not a detectable agent), so the readiness gate's total lookup needs this
/// list rather than that one — and `agent_readiness_001` asserts the two agree
/// about the five that overlap.
const EVERY_AGENT_TYPE: [AgentType; 6] = [
    AgentType::ClaudeCode,
    AgentType::OpenCode,
    AgentType::Pi,
    AgentType::Codex,
    AgentType::Devin,
    AgentType::None,
];

/// The classification each agent is pinned to, and the measurement it rests on.
///
/// Deliberately an exhaustive `match` rather than a lookup table: a new
/// `AgentType` variant does not compile here until somebody states what a fresh
/// instance of it announces before its first prompt. That is the whole point —
/// the value the gate reads must be a considered per-agent fact, and the failure
/// mode this file exists to prevent is a new adapter silently inheriting
/// whatever some unrelated field implies.
fn pinned_readiness(agent_type: &AgentType) -> PrePromptReadiness {
    match agent_type {
        // Claude Code posts its native `SessionStart` early in boot, before any
        // prompt — the delegate gate's healthy fast path, 3.80-4.39 s end to end
        // on #243's own production samples. Nothing else in the suite pins this
        // value, and it is the one the retired `hook_install.is_some()`
        // predicate agreed with, so a refactor that reintroduces the old
        // predicate stays green everywhere EXCEPT here and on OpenCode.
        AgentType::ClaudeCode => PrePromptReadiness::NativeSessionStart,
        // MEASURED in #146 against `opencode 1.18.16`: 35 s of idle cold boot
        // produced zero `session.*` events and `session.created` then landed
        // 16 ms AFTER the prompt was accepted — i.e. caused by the very prompt
        // the gate was withholding. A `Plugin` agent, so no wrapper can watch it
        // either; there is genuinely nothing to wait for.
        AgentType::OpenCode => PrePromptReadiness::NoSignal,
        // Pi emits no `EventType::SessionStart` at all (PRD #201), so `NoSignal`
        // is the LITERALLY TRUE classification — and is deliberately not claimed.
        // Pinning the current value, not correcting it: nobody has measured Pi's
        // boot window, #243 measured only Codex and OpenCode, and claiming
        // `NoSignal` would take the SCHEDULER's Pi delivery from a 30 s wait to a
        // ~1 s buffer on no evidence with no test on that path. The full argument
        // is in `PrePromptReadiness::Unknown`'s doc comment; reclassify when Pi's
        // boot is measured, and change this line in the same commit.
        AgentType::Pi => PrePromptReadiness::Unknown,
        // Codex's NATIVE `SessionStart` fires when the first TURN starts (measured
        // on 0.145.0, still true on 0.149.0), which is AFTER the prompt — useless
        // as a gate, and the reason `hook_install.is_some()` was wrong about it
        // despite Codex genuinely having native hooks. The deck's own wrapper
        // hosts its PTY and announces the child's interface instead.
        AgentType::Codex => PrePromptReadiness::WrapperInterfaceReady,
        // Devin documents a `SessionStart` hook and runs unwrapped, so the gate
        // simply waits for the genuine one. Like Claude, pinned by nothing else
        // in the suite — and unlike Claude it is DOCUMENTED, NOT MEASURED: it is
        // the one value of the six with no boot-window observation behind it
        // (#243 review finding 3), recorded as such in the registry itself. Being
        // wrong here costs a delegate the 30 s fallback rather than a lost
        // prompt, which is why it ships as the conservative classification rather
        // than as `Unknown`. Pinning it still has value — it makes a silent drift
        // to some other value visible — but this line is not evidence the way the
        // other four are, and the pin should be re-derived, not merely re-read,
        // once somebody measures Devin's boot.
        AgentType::Devin => PrePromptReadiness::NativeSessionStart,
        // Not an agent: an unrecognized command, where the deck does not know
        // what is in the pane. "We have not measured this" is not evidence that
        // skipping the wait is safe, so the placeholder keeps the conservative
        // wait (`orchestration/delegate/008`, `/011`, `scheduler/spawn/005` all
        // depend on that).
        AgentType::None => PrePromptReadiness::Unknown,
    }
}

/// Scenario: Read the registry entry for every agent the deck knows — Claude
/// Code, OpenCode, Pi, Codex, Devin and the neutral "no agent" placeholder — and
/// assert each carries the exact `pre_prompt_readiness` value issue #243
/// established for it, so the fact the readiness gate reads cannot drift
/// silently. The expectation table is an exhaustive `match`, so adding a new
/// `AgentType` fails to compile here until somebody states what a fresh instance
/// of it announces before its first prompt.
#[spec("agent/readiness/001")]
#[test]
fn agent_readiness_001_every_agent_pins_its_pre_prompt_fact() {
    for agent_type in EVERY_AGENT_TYPE {
        let spec = agent_registry::spec(&agent_type);
        assert_eq!(
            spec.agent_type, agent_type,
            "the registry lookup for {agent_type:?} returned another agent's entry"
        );
        assert_eq!(
            spec.pre_prompt_readiness,
            pinned_readiness(&agent_type),
            "{} no longer declares the pre-prompt readiness fact issue #243 established for it. \
             This value decides whether the delegate and scheduler gates wait for a \
             `SessionStart` or treat that wait as dead time, so changing it changes real \
             delivery latency — update the reasoning in `pinned_readiness` in the same commit \
             that changes the registry, or put it back.",
            spec.label
        );
    }

    // The shipped, detectable agents and the total lookup must agree about the
    // five they share. A new agent added to `ALL` without a line in
    // `EVERY_AGENT_TYPE` would otherwise be pinned by nothing at all — the exact
    // gap this test exists to close for Claude Code and Devin.
    assert_eq!(
        agent_registry::ALL.len(),
        EVERY_AGENT_TYPE.len() - 1,
        "a shipped agent was added to (or removed from) `agent_registry::ALL` without updating \
         `EVERY_AGENT_TYPE`; shipped = {:?}",
        agent_registry::ALL
            .iter()
            .map(|spec| spec.label)
            .collect::<Vec<_>>()
    );
    for spec in agent_registry::ALL {
        assert!(
            EVERY_AGENT_TYPE.contains(&spec.agent_type),
            "shipped agent {} is not covered by this file's readiness pins",
            spec.label
        );
    }
    assert!(
        !agent_registry::ALL
            .iter()
            .any(|spec| spec.agent_type == AgentType::None),
        "the neutral placeholder must stay out of `ALL` — it is not a detectable agent"
    );
}

/// Scenario: Assert the readiness gate's discriminator is the registry's own
/// `pre_prompt_readiness` field rather than the retired `hook_install.is_some()`
/// stand-in, by naming the shipped agents where the two disagree: OpenCode
/// carries a hook installer yet declares no pre-prompt signal at all, while Pi
/// and the neutral placeholder carry no hook installer yet still keep the gate
/// waiting. Also pins `has_signal()`'s full truth table, including that
/// `Unknown` answers `true` so an unmeasured agent keeps the conservative wait.
#[spec("agent/readiness/002")]
#[test]
fn agent_readiness_002_gate_discriminator_is_not_hook_install() {
    // The predicate the gates actually branch on. `false` is the ONLY value that
    // buys an agent the short path (the dead-wait skip), so `Unknown` answering
    // `true` is load-bearing, not an accident of the enum's shape.
    assert!(PrePromptReadiness::NativeSessionStart.has_signal());
    assert!(PrePromptReadiness::WrapperInterfaceReady.has_signal());
    assert!(
        !PrePromptReadiness::NoSignal.has_signal(),
        "`NoSignal` is the one positive declaration that there is nothing to wait for; if it \
         reports a signal, the 30 s dead wait issue #243 removed comes straight back"
    );
    assert!(
        PrePromptReadiness::Unknown.has_signal(),
        "an unmeasured agent must keep waiting: \"we do not know what this is\" is not evidence \
         that skipping the wait is safe (`orchestration/delegate/011`, `scheduler/spawn/005`)"
    );

    // THE COUNTER-EXAMPLE, and the reason the gate cannot read `hook_install`:
    // OpenCode ships a plugin installer AND announces nothing before its first
    // prompt. Under the retired predicate it was told a `SessionStart` was
    // coming, and burned the full timeout every time.
    let opencode = agent_registry::spec(&AgentType::OpenCode);
    assert!(
        opencode.hook_install.is_some(),
        "OpenCode must keep its plugin installer — this test's whole point is that HAVING one \
         says nothing about pre-prompt readiness"
    );
    assert!(
        !opencode.pre_prompt_readiness.has_signal(),
        "OpenCode declares no pre-prompt signal (#146), so the gate must not wait for one"
    );

    // THE CONVERSE, so the two predicates cannot be re-derived from one another
    // in either direction: hookless agents whose gate still waits.
    for agent_type in [AgentType::Pi, AgentType::None] {
        let spec = agent_registry::spec(&agent_type);
        assert!(
            spec.hook_install.is_none(),
            "{} is expected to carry no hook installer here",
            spec.label
        );
        assert!(
            spec.pre_prompt_readiness.has_signal(),
            "{} has no hook installer and must STILL keep the gate waiting — absence of hooks is \
             not a positive declaration that nothing will arrive",
            spec.label
        );
    }

    // Codex is the third shape: native hooks present, yet the pre-prompt fact is
    // the deck's own wrapper observing the child's interface, never Codex's
    // native `SessionStart` (which the prompt itself causes). A refactor that
    // collapsed readiness back onto "has native hooks" would lose this
    // distinction while still reporting a signal.
    let codex = agent_registry::spec(&AgentType::Codex);
    assert!(codex.hook_install.is_some());
    assert_eq!(
        codex.pre_prompt_readiness,
        PrePromptReadiness::WrapperInterfaceReady,
        "Codex's readiness comes from the wrapper watching its interface, not from its native \
         hooks (`codex/wrap/006`, `orchestration/delegate/029`)"
    );

    // Finally, state the disagreement as a whole-registry fact rather than three
    // separate examples: the set of agents with a hook installer is NOT the set
    // of agents the gate has something to wait for, and these are exactly the
    // agents where they part company.
    let disagreements: Vec<&'static str> = EVERY_AGENT_TYPE
        .iter()
        .map(agent_registry::spec)
        .filter(|spec| spec.hook_install.is_some() != spec.pre_prompt_readiness.has_signal())
        .map(|spec| spec.label)
        .collect();
    assert_eq!(
        disagreements,
        vec!["OpenCode", "Pi", "No agent"],
        "the two predicates must keep disagreeing on exactly these agents; if the list becomes \
         empty, `pre_prompt_readiness` has been quietly re-derived from `hook_install` and issue \
         #243's defect is back"
    );
}

/// Scenario: Build a `SessionStart` for each of the wrapper's three
/// `session_start_origin` values and one with no origin key at all, then ask all
/// four of `AgentEvent`'s origin predicates about each. The full truth table must
/// hold — in particular the settled marker must NOT answer the strong
/// raw-input-mode question, because that is the single bit deciding which of the
/// wrapper's two facts may RELEASE the readiness gate, and which post-readiness
/// buffer is then owed.
#[spec("agent/readiness/003")]
#[test]
fn agent_readiness_003_session_start_origin_predicates_price_the_two_facts_apart() {
    // (origin, fork?, interface-READY?, interface-SETTLED?)
    // The remaining two predicates are derived below rather than tabulated, so
    // a table typo cannot make them agree with themselves.
    let rows: [(Option<&str>, bool, bool, bool); 4] = [
        // No marker at all: every hook-emitting agent's own `SessionStart`, and
        // the shape a pre-#243 wrapper still posts. None of the wrapper
        // questions may answer `true` for it.
        (None, false, false, false),
        // Boot provenance: the wrapper saying "I forked a child", which says
        // nothing about the child's interface.
        (Some(WRAPPER_FORK_SESSION_START_ORIGIN), true, false, false),
        // Fact 1, the OBSERVATION: the child cleared `ICANON`/`ECHO`. The one
        // value a Wrapper-strategy agent's gate may be released on before its
        // upgrade window expires, and the one priced at
        // `WRAPPER_INTERFACE_READINESS_BUFFER` rather than the ordinary buffer.
        // It used to be the one value that SKIPPED the buffer; measurement
        // retracted that (a TUI takes raw mode at INIT, before it will accept a
        // submit), and the predicates below are unchanged by the retraction.
        (
            Some(WRAPPER_INTERFACE_READY_SESSION_START_ORIGIN),
            false,
            true,
            false,
        ),
        // Fact 2, the GUESS: the child wrote something and then went quiet for
        // the settle window. A stalled launcher settles exactly like a REPL at
        // its prompt, so this must stay OUT of the strong predicate.
        (
            Some(WRAPPER_INTERFACE_SETTLED_SESSION_START_ORIGIN),
            false,
            false,
            true,
        ),
    ];

    for (origin, fork, interface_ready, interface_settled) in rows {
        let event = session_start_with_origin(origin);
        assert_eq!(
            event.is_wrapper_fork_session_start(),
            fork,
            "origin {origin:?}: is_wrapper_fork_session_start"
        );
        assert_eq!(
            event.is_wrapper_interface_ready_session_start(),
            interface_ready,
            "origin {origin:?}: is_wrapper_interface_ready_session_start — this is the bit the \
             delegate's buffer skip reads (`src/state.rs`, guard 1), so widening it reprices the \
             seam"
        );
        assert_eq!(
            event.is_wrapper_interface_settled_session_start(),
            interface_settled,
            "origin {origin:?}: is_wrapper_interface_settled_session_start"
        );
        // The two composites are definitionally the unions, and stating them
        // that way pins the RELATIONSHIP rather than four independent booleans:
        // whichever value is added next, the composites must still be the unions
        // of the narrow answers, and `is_wrapper_session_start` must still be
        // the one that also admits the fork marker.
        assert_eq!(
            event.is_wrapper_interface_session_start(),
            interface_ready || interface_settled,
            "origin {origin:?}: the EITHER-fact predicate must be exactly the union of the two \
             narrow ones — it is what releases the readiness GATE, which both facts may do"
        );
        assert_eq!(
            event.is_wrapper_session_start(),
            fork || interface_ready || interface_settled,
            "origin {origin:?}: the any-wrapper predicate must admit the fork marker as well as \
             both interface facts"
        );
    }

    // An unrecognised value must read as "not a wrapper marker" rather than as
    // any of the three: a forward-compatible producer inventing a fourth origin
    // must not inherit the privilege the strong one carries.
    let unknown = session_start_with_origin(Some("wrapper_interface_ready_v2"));
    assert!(
        !unknown.is_wrapper_session_start(),
        "an unrecognised origin value must not satisfy ANY wrapper predicate; prefix matching \
         here would hand a future or hostile value the buffer skip"
    );
}
