//! PRD #1666 part 3 (Q35): the composer probe — how the deck checks an
//! agent's input box through the agent's own external-editor key (Ctrl+G)
//! instead of following its keystrokes, and holds what a user types while a
//! check runs.
//!
//! Built a commit at a time (Q35-core's implementation plan). Today it holds
//! the framing tokenizer ([`framing`]), the output hook with its
//! bracketed-paste tracker ([`paste_mode`]), the rule for trusting the
//! file an agent exports ([`path_rule`]), the classifier of whose editor
//! invocation it is ([`classifier`]), the daemon's own attribution of an
//! invocation and its read of the export ([`attest`], Linux), the arm page
//! the daemon publishes to its editor helper ([`arm`]), the permission to
//! arm ([`permission`]), the input epoch with the query ledger, `B` and the
//! native hazard FIFO kept beside it ([`epoch`], [`query_ledger`]) and the
//! estimate of when an agent's redraw waits run out ([`query_wait`]), a
//! generation's invocation records, what each settles and its writer scope
//! ([`record`]), the segments each check or held Ctrl+G opens and what
//! releases and completes them ([`segments`]), and installation: the
//! decision at each spawn ([`install`]) and the instance directory it
//! installs from ([`instance`], Linux), and the editor helper an installed
//! agent runs as its editor ([`helper`], Linux), which the binary dispatches
//! to by its name, and the daemon's side of that helper: its invocations,
//! markers, children and exits, which settle the generation's liabilities
//! ([`observe`]), whether an agent's key-binding sources still leave
//! Ctrl+G on its external editor ([`keymap`], no caller yet), and the fence
//! gate every attach frame passes through on its way to the PTY ([`gate`]),
//! which holds a person's input while a fence is closed and lets a reply to a
//! later query pass ahead, and the two framings of a pane's input: the
//! planner that cuts or numbers each Ctrl+G candidate at acceptance and the
//! tracker of what the PTY took, at whose boundary alone a deck line starts
//! ([`planner`]), and probe admission: the one predicate every check write
//! asks ([`admission`]), the probe job's start on the writer thread with the
//! arm published only inside it, and the retry schedule ([`probe`]), which
//! the daemon's probe request and doorbell tick use (Q35-20), and the drain frontier's request
//! queue and wake ([`frontier`]), which a generation's Linux PTY reader takes
//! frontiers from, with the check that no other process holds its master
//! ([`master_holders`], Linux). The daemon feeds the hook every byte an agent writes,
//! moves the epoch at acceptance, and decides installation at
//! every spawn, for the classes `install::INSTALL_ROLLOUT` names (a direct
//! Claude Code, a direct or launched Codex) and, in the `e2e` build, the
//! classes its assumption adds. For an installed generation it keeps the
//! invocation records, settles the permission to arm, answers its helper and
//! observes it. A probe on an `Armable` generation publishes a liability and
//! closes a fence (Q35-20), and its segments release what the fence held.
//! Whether a ring
//! follows a check still reads today's draft tracking until Q35-21. The
//! planner reads the records' liability count, to cut a person's input while
//! one is out.

pub mod admission;
pub mod arm;
#[cfg(target_os = "linux")]
pub mod attest;
pub mod classifier;
#[cfg(target_os = "linux")]
pub mod discovery;
pub mod epoch;
pub mod framing;
pub mod frontier;
pub mod gate;
#[cfg(target_os = "linux")]
pub mod helper;
pub mod install;
#[cfg(target_os = "linux")]
pub mod instance;
pub mod keymap;
#[cfg(target_os = "linux")]
pub mod master_holders;
pub mod observe;
pub mod paste_mode;
pub mod path_rule;
pub mod planner;
pub mod probe;
pub mod query_ledger;
pub mod query_wait;
pub mod record;
pub mod recovery;
pub mod segments;
#[cfg(test)]
pub(crate) mod stamp_park;

use crate::doorbell_activation::{AgentFacts, EditorProbe, LatchedRelease, Measurement};
use crate::event::AgentType;

/// Where a generation's agent root `A` stands: bound, still being looked for,
/// or never to be bound in this generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    Waiting,
    Bound,
    Never,
}

/// Whether the deck may arm the probe on a generation (Q35 plan §1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// The header or the binding is not known yet.
    Pending,
    /// The agent's `editor_probe` facts are verified and `A` is bound.
    Armable,
    /// Not for this generation's life.
    Inert(InertCause),
}

/// Why a generation is [`Permission::Inert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InertCause {
    /// The header latched no single release.
    UnknownRelease,
    /// The deck has no facts for this agent (OpenCode, Pi, Devin).
    UnsupportedAgent,
    /// The agent's facts have no verified `editor_probe` (no shipped
    /// agent's since the Codex activation; a test's own facts can).
    NoEditorRecord,
    /// `A` was never bound.
    Unbound,
    /// Codex's export directory was refused before the spawn (Q35-27,
    /// `install::prepare_codex_export`), so every read of it would be.
    ExportDirectory,
}

/// The permission half of plan §1.2, asked when the generation's header
/// arrives and when `A` is bound: `header` is `None` until the release
/// window closes. The header must latch a single release (any one: it is not
/// compared with anything), and the agent's facts in `facts` must have a
/// verified `editor_probe`, or `assumed` must name one for exactly that agent
/// and release (the e2e stand-ins'); a cause known from either input settles
/// the generation as inert even while the other is pending.
///
/// This is the observer's permission, not the ring's. Both read the agent's
/// facts, but permission needs none of the ring's, and the ring's activation
/// doesn't read `editor_probe`. The checks a later commit adds (the path, the
/// keymap, the namespace, the liability cap) narrow `Armable` further.
pub fn permission(
    facts: &[AgentFacts<'_>],
    assumed: &[(AgentType, semver::Version, EditorProbe<'_>)],
    agent: &AgentType,
    header: Option<&LatchedRelease>,
    binding: Binding,
) -> Permission {
    if let Some(header) = header {
        let LatchedRelease::Known(release) = header else {
            return Permission::Inert(InertCause::UnknownRelease);
        };
        let probe = crate::doorbell_activation::editor_probe_for(facts, assumed, agent, release);
        if crate::doorbell_activation::facts_for(facts, agent).is_none()
            && !matches!(probe, Measurement::Verified(_))
        {
            return Permission::Inert(InertCause::UnsupportedAgent);
        }
        if !matches!(probe, Measurement::Verified(_)) {
            return Permission::Inert(InertCause::NoEditorRecord);
        }
    }
    match (header, binding) {
        (_, Binding::Never) => Permission::Inert(InertCause::Unbound),
        (None, _) | (_, Binding::Waiting) => Permission::Pending,
        (Some(_), Binding::Bound) => Permission::Armable,
    }
}
