//! Fast-tier, pure-data coverage for the per-agent pre-prompt readiness fact
//! (issue #243).
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
//! `orchestration/delegate/024`, `/025` and `codex/wrap/006`. What is covered
//! HERE is the data those tests happen to exercise, per agent and
//! exhaustively — because behavioural coverage is necessarily partial: `/025`
//! pins OpenCode, `/007` pins Codex, `/008` and `/011` pin the neutral
//! placeholder, and nothing at all pins Claude Code or Devin, whose values are
//! precisely the ones a careless refactor of the old `hook_install` predicate
//! would get wrong.

use dot_agent_deck::agent_registry::{self, PrePromptReadiness};
use dot_agent_deck::event::AgentType;
use spec::spec;

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
         hooks (`codex/wrap/006`, `orchestration/delegate/024`)"
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
