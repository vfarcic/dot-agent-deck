//! PRD #155 — centralized color palette (single source of truth).
//!
//! Before this module the TUI's semantic colors were scattered as inline
//! `Color::X` literals across the deck-card and embedded-pane render paths,
//! and the two surfaces drifted apart (a working agent could look different as
//! a deck card vs. as an embedded pane). This palette names the semantic
//! **roles** once and both render paths resolve their colors through it, so a
//! given state renders identically everywhere (PRD #155 Option A).
//!
//! ## Border policy (Option A — identical in both render paths)
//!
//! The card/pane border encodes **STATUS** in both the dashboard deck and the
//! embedded panes. Selection and focus are conveyed by dedicated **accent**
//! roles that never reuse a status color, so status / selection / focus are
//! always visually distinct. The unified border-resolution precedence is:
//!
//! 1. **selected** → [`SELECTED`] (Magenta) + BOLD + the `▸ ` title marker.
//! 2. else **focused** → [`FOCUSED`] (Cyan).
//! 3. else → the agent's **status** role ([`status_color`]).
//!
//! The per-card status **badge** always shows status, so the accent override
//! in (1)/(2) never loses status information.
//!
//! All roles are **named ANSI** colors only — no absolute `Color::Rgb`, which
//! the theme guards (`theme/contrast/001`) forbid so terminal themes can remap
//! them.

use ratatui::style::Color;

use crate::state::SessionStatus;

// ---------------------------------------------------------------------------
// Status roles
// ---------------------------------------------------------------------------

/// Working — the agent is actively running a tool / producing output.
pub const STATUS_WORKING: Color = Color::Green;
/// Thinking — the agent is reasoning before acting.
pub const STATUS_THINKING: Color = Color::Blue;
/// Waiting — the agent needs user input to proceed.
pub const STATUS_WAITING: Color = Color::Yellow;
/// Error — the agent hit a failure.
pub const STATUS_ERROR: Color = Color::Red;
/// Idle — no current activity (dimmed).
pub const IDLE: Color = Color::DarkGray;

// ---------------------------------------------------------------------------
// Accent roles (must be distinct from every status color and from each other)
// ---------------------------------------------------------------------------

/// The focused embedded pane. Cyan was previously used for both focus and
/// selection; Option A keeps focus on Cyan and moves selection to [`SELECTED`]
/// so the two are provably distinct.
pub const FOCUSED: Color = Color::Cyan;
/// The selected deck card (rendered BOLD with a `▸ ` title marker). Magenta is
/// free — status uses green/blue/yellow/red and focus uses cyan — so selection
/// never collides with a status color or with focus (PRD #155 criterion #3).
pub const SELECTED: Color = Color::Magenta;

/// Resolve a session status to its centralized border/badge role color. This
/// is the single source of truth shared by the deck-card render path
/// (`src/ui.rs`) and the embedded-pane render path (`src/terminal_widget.rs`),
/// so a given state shows the same border color in both contexts.
pub fn status_color(status: &SessionStatus) -> Color {
    match status {
        SessionStatus::Working => STATUS_WORKING,
        SessionStatus::Thinking => STATUS_THINKING,
        // Compacting is a thinking-adjacent transient state; it shares the
        // thinking role rather than introducing a sixth status color.
        SessionStatus::Compacting => STATUS_THINKING,
        SessionStatus::WaitingForInput => STATUS_WAITING,
        SessionStatus::Error => STATUS_ERROR,
        SessionStatus::Idle => IDLE,
    }
}
