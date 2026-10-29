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
//! native hazard FIFO kept beside it ([`epoch`], [`query_ledger`]), a
//! generation's invocation records, what each settles and its writer scope
//! ([`record`]), the segments each check or held Ctrl+G opens and what
//! releases and completes them ([`segments`]), and installation: the
//! decision at each spawn ([`install`]) and the instance directory it
//! installs from ([`instance`], Linux), and the editor helper an installed
//! agent runs as its editor ([`helper`], Linux), which the binary dispatches
//! to by its name, and the daemon's side of that helper: its invocations,
//! markers, children and exits, which settle the generation's liabilities
//! ([`observe`]), and whether an agent's key-binding sources still leave
//! Ctrl+G on its external editor ([`keymap`], no caller yet). The daemon feeds the hook every byte an agent writes,
//! moves the epoch at the PTY writer's enqueue, and decides installation at
//! every spawn, which installs nothing outside the `e2e` build's assumption
//! until activation. For an installed generation it keeps the invocation
//! records, settles the permission to arm, answers its helper and observes
//! it; nothing publishes a liability outside the `e2e` build's control verb,
//! and nothing reads the epoch, the records or a segment to admit or hold
//! anything yet.

pub mod arm;
#[cfg(target_os = "linux")]
pub mod attest;
pub mod classifier;
pub mod epoch;
pub mod framing;
#[cfg(target_os = "linux")]
pub mod helper;
pub mod install;
#[cfg(target_os = "linux")]
pub mod instance;
pub mod keymap;
pub mod observe;
pub mod paste_mode;
pub mod path_rule;
pub mod query_ledger;
pub mod record;
pub mod segments;

use crate::doorbell_activation::{LatchedRelease, Measurement, ReleaseRecord};
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
    /// The release's `editor_probe` record is verified and `A` is bound.
    Armable,
    /// Not for this generation's life.
    Inert(InertCause),
}

/// Why a generation is [`Permission::Inert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InertCause {
    /// The header latched no single release.
    UnknownRelease,
    /// No record covers exactly this release. Nothing inherits a record.
    UnmeasuredRelease,
    /// The release's record has no verified `editor_probe`.
    NoEditorRecord,
    /// `A` was never bound.
    Unbound,
}

/// The permission half of plan §1.2, asked when the generation's header
/// arrives and when `A` is bound: `header` is `None` until the release
/// window closes. The release comes only from `header`, and only an exact
/// record with a verified `editor_probe` permits; a cause known from either
/// input settles the generation as inert even while the other is pending.
///
/// This is the observer's permission, not the ring's. Both look a record up
/// by the same agent and release, but permission needs none of the ring's
/// measurements, and the ring's activation doesn't read `editor_probe`. The
/// checks a later commit adds (the path, the keymap, the namespace, the
/// liability cap) narrow `Armable` further.
pub fn permission(
    records: &[ReleaseRecord<'_>],
    agent: &AgentType,
    header: Option<&LatchedRelease>,
    binding: Binding,
) -> Permission {
    if let Some(header) = header {
        let LatchedRelease::Known(release) = header else {
            return Permission::Inert(InertCause::UnknownRelease);
        };
        let Some(record) = records
            .iter()
            .find(|r| r.agent == *agent && r.release == *release)
        else {
            return Permission::Inert(InertCause::UnmeasuredRelease);
        };
        if !matches!(record.editor_probe, Measurement::Verified(_)) {
            return Permission::Inert(InertCause::NoEditorRecord);
        }
    }
    match (header, binding) {
        (_, Binding::Never) => Permission::Inert(InertCause::Unbound),
        (None, _) | (_, Binding::Waiting) => Permission::Pending,
        (Some(_), Binding::Bound) => Permission::Armable,
    }
}
