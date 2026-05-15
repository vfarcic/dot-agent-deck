use std::any::Any;
use std::sync::Arc;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PaneError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Command failed: {0}")]
    CommandFailed(String),
    #[error("Parse error: {0}")]
    ParseError(String),
    #[error("Pane control not available")]
    NotAvailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub title: String,
    pub is_focused: bool,
    pub command: Option<String>,
}

/// Outcome of a [`PaneController::rename_pane`] call.
///
/// M2.11 fixup 5 — the rename path now returns what the controller
/// actually did with the user-supplied text, so callers (the dashboard
/// rename handler in particular) can mirror the EXACT label the
/// controller stored on `Pane.name` (and queued for the daemon) into
/// `ui.display_names` / `ui.pane_display_names`. Before this, the UI
/// inserted the raw `ui.rename_text` verbatim, which diverged from
/// the controller's trim + `is_valid_display_name` normalization (a
/// `"  newname  "` rename left the dashboard map padded while the
/// daemon stored `"newname"`, and a control-byte rename slipped
/// terminal escapes into the dashboard card title even though the
/// controller refused the change). This extends fixup 4's "single
/// source of truth" pattern from create to rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameOutcome {
    /// Controller accepted the rename; the included string is the
    /// canonical label now stored on local `Pane.name` (and, for
    /// stream-backed panes, queued for the daemon via
    /// `set_agent_label`). Callers must mirror this exact string
    /// into their UI display-name maps.
    ///
    /// **Invariant**: the inner `String` must be the trimmed value of
    /// the user's input and must satisfy
    /// [`crate::agent_pty::is_valid_display_name`]. The only correct
    /// way to construct this variant outside a context that has just
    /// validated the label itself (the production controller, or a
    /// test using a statically known-valid literal) is via
    /// [`RenameOutcome::applied`] — which trims, validates, and falls
    /// back to [`RenameOutcome::Cleared`] / [`RenameOutcome::Rejected`]
    /// for invalid input. Bypassing the constructor with raw user
    /// input would allow padded or control-byte labels into UI
    /// display-name maps and break the fixup-5 normalization
    /// guarantee (PRD #76 M2.11 fixup-5 auditor LOW).
    Applied(String),
    /// Controller treated the rename as a "clear" — empty or
    /// whitespace-only input. Local `Pane.name` is now empty and the
    /// daemon (if any) will be sent `display_name: None` so hydrate
    /// falls back to the agent_id on reconnect rather than restoring
    /// a stale label. Callers should remove the corresponding entry
    /// from their UI display-name maps.
    Cleared,
    /// Controller refused the rename because the trimmed value
    /// failed `is_valid_display_name` (control bytes, oversized,
    /// etc.). Neither local `Pane.name` nor the daemon was mutated;
    /// callers must leave their UI display-name maps unchanged so
    /// the previous label stays visible.
    Rejected,
}

impl RenameOutcome {
    /// Construct a [`RenameOutcome`] from raw user-supplied rename text,
    /// applying the same trim + validation rules the production
    /// controller (`EmbeddedPaneController::rename_pane`) uses. This is
    /// the single canonical way to derive a `RenameOutcome` from
    /// untrusted input: it guarantees the [`Applied`](Self::Applied)
    /// invariant by routing invalid bytes to [`Rejected`](Self::Rejected)
    /// and empty/whitespace-only input to [`Cleared`](Self::Cleared).
    ///
    /// Mock `PaneController::rename_pane` implementations should call
    /// this instead of constructing `Applied(name.to_string())` directly
    /// — otherwise a test driving invalid input through a mock would
    /// produce an `Applied` value that violates the documented variant
    /// invariant and could falsely prove the UI accepts unvalidated
    /// labels (PRD #76 M2.11 fixup-5 auditor LOW).
    ///
    /// Semantics, matching the production controller:
    /// - Trim the input.
    /// - Empty after trim → [`Cleared`](Self::Cleared).
    /// - Non-empty + [`crate::agent_pty::is_valid_display_name`] →
    ///   [`Applied`](Self::Applied) with the trimmed label.
    /// - Non-empty + invalid → [`Rejected`](Self::Rejected).
    pub fn applied(name: impl AsRef<str>) -> Self {
        let trimmed = name.as_ref().trim();
        if trimmed.is_empty() {
            Self::Cleared
        } else if crate::agent_pty::is_valid_display_name(trimmed) {
            Self::Applied(trimmed.to_string())
        } else {
            Self::Rejected
        }
    }
}

pub trait PaneController: Send + Sync {
    fn focus_pane(&self, pane_id: &str) -> Result<(), PaneError>;
    fn create_pane(&self, command: Option<&str>, cwd: Option<&str>) -> Result<String, PaneError>;
    /// Create a pane and apply a display name in one step. The default impl
    /// composes `create_pane` + `rename_pane`, which is fine for local-PTY
    /// backends. The stream-backed `EmbeddedPaneController` overrides this
    /// so the name reaches the daemon via `StartAgent.display_name` —
    /// otherwise a disconnect or crash between `create_pane` and
    /// `rename_pane` would persist the command-based fallback name on the
    /// daemon and lose the user's chosen Name on the next reconnect
    /// (PRD #76 M2.11 reviewer P2).
    ///
    /// Returns `(pane_id, resolved_display_name)` so the caller can mirror
    /// the EXACT label the controller (and the daemon, for stream panes)
    /// stored. The resolved name is computed via
    /// [`crate::agent_pty::resolve_display_name`] — the single source of
    /// truth shared with the UI maps, preventing the divergence between
    /// `ui.pane_display_names` and `AgentRecord.display_name` that fixup-3
    /// reviewer P2 / auditor LOW called out.
    fn create_pane_with_display_name(
        &self,
        command: Option<&str>,
        cwd: Option<&str>,
        display_name: Option<&str>,
    ) -> Result<(String, String), PaneError> {
        let resolved = crate::agent_pty::resolve_display_name(display_name, command);
        let id = self.create_pane(command, cwd)?;
        // Discard the RenameOutcome here: `resolved` is already the
        // canonical resolved value (computed via the shared resolver),
        // so the controller's outcome adds no new information for the
        // create-then-rename path. The new outcome shape matters at
        // the dashboard rename call site, not here.
        self.rename_pane(&id, &resolved)?;
        Ok((id, resolved))
    }
    fn close_pane(&self, pane_id: &str) -> Result<(), PaneError>;
    fn list_panes(&self) -> Result<Vec<PaneInfo>, PaneError>;
    fn resize_pane(
        &self,
        pane_id: &str,
        direction: PaneDirection,
        amount: u16,
    ) -> Result<(), PaneError>;
    /// Rename a pane. Returns a [`RenameOutcome`] describing what the
    /// controller actually did so callers (the dashboard rename
    /// handler in particular) can mirror the controller-resolved
    /// label into their UI display-name maps instead of inserting
    /// the raw user input verbatim. See [`RenameOutcome`] for the
    /// three states.
    fn rename_pane(&self, pane_id: &str, name: &str) -> Result<RenameOutcome, PaneError>;
    fn toggle_layout(&self) -> Result<(), PaneError>;
    fn write_to_pane(&self, pane_id: &str, text: &str) -> Result<(), PaneError>;
    fn name(&self) -> &str;
    fn is_available(&self) -> bool;
    fn as_any(&self) -> &dyn Any;
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

pub fn detect_multiplexer() -> Arc<dyn PaneController> {
    Arc::new(crate::embedded_pane::EmbeddedPaneController::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_multiplexer_returns_embedded() {
        let ctrl = detect_multiplexer();
        assert_eq!(ctrl.name(), "embedded");
        assert!(ctrl.is_available());
    }

    // M2.11 fixup 6 — pin the typed `RenameOutcome::applied` constructor
    // semantics. These tests are what makes the constructor a meaningful
    // type-level guarantee: callers (production controller + mocks) can
    // route raw user input through one function and trust the three
    // documented outcomes, instead of repeating the trim + validation
    // dance and risking divergence (PRD #76 M2.11 fixup-5 auditor LOW).

    #[test]
    fn rename_outcome_applied_returns_applied_for_valid_input() {
        assert_eq!(
            RenameOutcome::applied("foo"),
            RenameOutcome::Applied("foo".to_string())
        );
    }

    #[test]
    fn rename_outcome_applied_trims_surrounding_whitespace() {
        // Matches the production controller's trim semantics so the
        // dashboard map can't end up storing a padded label.
        assert_eq!(
            RenameOutcome::applied("  foo  "),
            RenameOutcome::Applied("foo".to_string())
        );
        assert_eq!(
            RenameOutcome::applied("\tbar\n"),
            RenameOutcome::Applied("bar".to_string())
        );
    }

    #[test]
    fn rename_outcome_applied_rejects_control_bytes() {
        // ANSI ESC after trim — the canonical "garbage that would slip
        // into the dashboard title" case. Must NOT become Applied.
        assert_eq!(RenameOutcome::applied("\x1b[31m"), RenameOutcome::Rejected);
        assert_eq!(
            RenameOutcome::applied("  \x1b[31mevil  "),
            RenameOutcome::Rejected
        );
    }

    #[test]
    fn rename_outcome_applied_treats_empty_as_cleared() {
        assert_eq!(RenameOutcome::applied(""), RenameOutcome::Cleared);
    }

    #[test]
    fn rename_outcome_applied_treats_whitespace_only_as_cleared() {
        // User's "clear" intent — must map to Cleared so the daemon-side
        // field is cleared rather than stored as a blank label.
        assert_eq!(RenameOutcome::applied("   "), RenameOutcome::Cleared);
        assert_eq!(RenameOutcome::applied("\t \n"), RenameOutcome::Cleared);
    }

    #[test]
    fn rename_outcome_applied_rejects_oversized_label() {
        // Mirror the daemon-side cap (`is_valid_display_name` rejects
        // > DISPLAY_NAME_MAX_LEN = 128 bytes after trim). A 129-byte
        // payload must surface as Rejected so the UI never mirrors a
        // label the daemon would refuse to store — otherwise the
        // dashboard map and `AgentRecord.display_name` would diverge.
        let oversized = "a".repeat(crate::agent_pty::DISPLAY_NAME_MAX_LEN + 1);
        assert_eq!(RenameOutcome::applied(&oversized), RenameOutcome::Rejected);
    }

    #[test]
    fn rename_outcome_applied_accepts_unicode_label() {
        // `is_valid_display_name` allows bytes ≥ 0x20 (control bytes
        // and DEL are the only thing filtered), so legitimate UTF-8
        // labels — kanji, emoji, accented Latin — must round-trip as
        // Applied with the trimmed bytes preserved. Pads with
        // surrounding whitespace to exercise the trim step too.
        assert_eq!(
            RenameOutcome::applied("  café-агент-日本語  "),
            RenameOutcome::Applied("café-агент-日本語".to_string())
        );
    }
}
