//! The TUI's upgrade dialog (issue #1635): the plans [`crate::self_upgrade`]
//! makes for this machine's copies, shown in the core's own words, and each
//! actionable one offered behind its own confirmation.
//!
//! STUB: the contract the tests in `tests/render_upgrade_dialog.rs` pin; the
//! bodies are filled in by the implementation commit.

use std::time::Duration;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::self_upgrade::{Outcome, PlanLine, UpgradeError, UpgradePlan};

/// What the last check found: this TUI's copy first, then the other copy on
/// the machine when one was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeCheck {
    pub plans: Vec<UpgradePlan>,
}

impl UpgradeCheck {
    /// The badge's text: the headline of the first copy that is behind.
    pub fn notice(&self) -> Option<String> {
        None
    }
}

/// The badge as the footer draws it: the notice and the key that opens the
/// dialog.
pub fn badge_text(_notice: &str, _key: &str) -> String {
    String::new()
}

/// A button in the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeChoice {
    Cancel,
    Upgrade,
    Close,
}

/// Which step the dialog is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Asking whether to upgrade the plan at this index.
    Confirm(usize),
    /// The plan at this index is being carried out.
    Running(usize),
    /// Nothing is left to ask.
    Done,
}

/// What the TUI must do after a key or a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Close,
    /// Carry out the plan at this index, off the render thread.
    Run(usize),
}

/// What one upgrade did, in the core's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub ok: bool,
    pub lines: Vec<PlanLine>,
}

impl RunResult {
    /// A finished upgrade of `plan`. `this_tui` says whether the copy is the
    /// one this TUI runs from.
    pub fn from_outcome(_plan: &UpgradePlan, _outcome: &Outcome, _this_tui: bool) -> Self {
        Self {
            ok: true,
            lines: Vec::new(),
        }
    }

    /// A failed upgrade.
    pub fn from_error(_error: &UpgradeError) -> Self {
        Self {
            ok: false,
            lines: Vec::new(),
        }
    }
}

/// The dialog's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeDialog {
    plans: Vec<UpgradePlan>,
}

impl UpgradeDialog {
    pub fn new(plans: Vec<UpgradePlan>) -> Self {
        Self { plans }
    }

    pub fn plans(&self) -> &[UpgradePlan] {
        &self.plans
    }

    pub fn phase(&self) -> Phase {
        Phase::Done
    }

    pub fn selected(&self) -> UpgradeChoice {
        UpgradeChoice::Close
    }

    pub fn handle_key(&mut self, _key: KeyEvent) -> Effect {
        Effect::None
    }

    pub fn choose(&mut self, _choice: UpgradeChoice) -> Effect {
        Effect::None
    }

    pub fn finish(&mut self, _index: usize, _result: RunResult) {}
}

/// Draw `dialog` centred over the frame; returns each button's rectangle.
pub fn render(_frame: &mut Frame, _dialog: &UpgradeDialog) -> Vec<(UpgradeChoice, Rect)> {
    Vec::new()
}

/// How long the TUI waits between checks.
pub fn recheck_interval() -> Duration {
    crate::self_upgrade::UPDATE_RECHECK_INTERVAL
}
