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
//! embedded panes, unless something more urgent claims it. The unified
//! border-resolution precedence is:
//!
//! 1. **selected** (deck cards only) → [`SELECTED`] (`Color::Reset`, the
//!    terminal's own foreground) + `BorderType::Thick` + the `▸ ` title marker.
//! 2. else **focused AND live** (embedded panes only) → [`FOCUSED`] (Cyan).
//! 3. else → the agent's **status** role ([`status_color`]).
//!
//! Selection wins outright because it must be findable at a glance whatever the
//! agent is doing, and a status colour cannot guarantee that: [`STATUS_IDLE`] is
//! DarkGray, which on a dark background is barely distinguishable from the
//! background itself (issue #442). Three cues ride together — high-contrast
//! colour, heavier glyph, `▸ ` marker — because each of the first two failed on
//! its own in a real report. The full ladder, including how the command /
//! `PaneInput` distinction rides on emphasis, lives in exactly one place:
//! `ui::card_border_glyph` plus its caller.
//!
//! The per-card status **badge** always shows status, so the overrides in (1)
//! and (2) never lose status information.
//!
//! ### Why (2) requires "live" (issue #88 follow-up)
//!
//! For an **embedded pane**, "focused" and "keystrokes reach it" are different
//! facts: in command mode the focused pane is still the one `Ctrl+D` / `Enter`
//! return you to, but it accepts no keys. The Cyan accent originally rendered
//! in both cases, which made the loudest border signal on screen claim "type
//! here" while the keyboard was driving the deck — the mode was invisible on a
//! full-screen mode tab, where nothing else in the frame changes.
//!
//! So for panes, (2) applies only in `UiMode::PaneInput`
//! (`TerminalWidget::with_input_active`). In command mode the focused pane
//! falls through to (3) and reports its agent's status like any other pane,
//! while **border thickness** (`BorderType::Thick`) carries focus instead.
//! Colour answers "are my keystrokes landing here?", thickness answers "which
//! pane is focused?" — one channel each, no longer competing.
//!
//! ### Why a deck card is mode-aware too (PRD #341 M4, revised by issue #442)
//!
//! A card carries no cursor and takes no keystrokes of its own, so it has no
//! input mode in the sense (2) means. But *the deck* does: in command mode the
//! keyboard drives the cards, and in `UiMode::PaneInput` it drives a pane while
//! the selection merely persists. The selection cue rendered identically in
//! both, which left the Dashboard — where the pane overlay is weakest and the
//! user's eyes are on the deck — looking equally live either way.
//!
//! `UiMode` is therefore threaded into card rendering as well. PRD #341 M4
//! originally spent the SELECTION COLOUR on this — Magenta + BOLD in command
//! mode, Magenta + `Modifier::DIM` in `UiMode::PaneInput` — and issue #442
//! removed that, because DIM Magenta on a dark theme is indistinguishable from
//! [`STATUS_IDLE`]. (The freed slot is now [`STATUS_WAITING`] — see issue #579
//! for why that role had to leave Yellow.) Mode now rides on **emphasis**
//! alone: BOLD in command mode, nothing in `PaneInput`, and **never DIM**.
//! De-emphasis by fading is banned on this path outright; the selected card
//! must never be harder to see than an unselected one. See
//! `ui::card_border_glyph`.
//!
//! Thickness accompanies the colour rather than replacing it because the
//! 16-colour-safe palette has no spare *hue* (green/blue/magenta/red are
//! statuses, cyan is focus) and the remaining candidates are grays — the exact
//! light-background hazard PRD #13 exists to prevent. [`SELECTED`] sidesteps
//! that by not being a hue at all. `BorderType` never feeds `Block::inner`, so
//! the thicker glyph costs no layout: the pane's inner area, its PTY size, the
//! card's inner area, and the PRD #84 invariant-3 contract are all unaffected.
//!
//! All roles are **named ANSI** colors (or `Color::Reset`) only — no absolute
//! `Color::Rgb`, which the theme guards (`theme/contrast/001`) forbid so
//! terminal themes can remap them. Note that a *named* colour is not
//! automatically safe either: `Color::White` would be as invisible on a light
//! theme as `Color::DarkGray` is on a dark one. Where a role must contrast on
//! BOTH *and* may carry no hue, `Color::Reset` is the only correct answer,
//! because the terminal resolves it against its own background.
//!
//! A role that must keep a hue has no such escape, so it is chosen by
//! measurement instead: see [`STATUS_WAITING`] for the luminance ceiling every
//! such colour is up against (issue #579), and `theme/contrast/002` for the
//! guard that recomputes it.

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
///
/// ## Why Magenta and not Yellow (issue #579)
///
/// This role was `Color::Yellow`, the single worst named ANSI slot to put on a
/// light terminal. Measured on a real deck rasterised against white:
/// **1.70:1** for the plain slot, and **1.07:1** once a terminal renders the
/// badge's `Modifier::BOLD` as the bright variant (`#FFFF00` on white) —
/// against WCAG AA's 4.5:1 minimum, with 1.07:1 about as close to invisible as
/// two colours get. Because every status surface resolves through
/// [`status_color`], that one constant made the deck card's `Needs Input`
/// badge and border, the embedded pane's border, the stats bar's waiting
/// segment and the orchestration tab labels illegible together — which is why
/// the fix belongs here and not at any call site.
///
/// ### The constraint, and why the choice is forced
///
/// WCAG contrast is `(L_hi + 0.05) / (L_lo + 0.05)`, so a foreground that must
/// clear AA on white AND on black at once has to sit in a very narrow
/// luminance band. The best any colour can do is the point where both sides
/// are equal — `L = sqrt(0.0525) - 0.05 ≈ 0.1791`, giving **4.58:1**. That is
/// a hard ceiling for *every* colour, not just the ANSI ones, and it is barely
/// above the bar; there is no comfortable answer to find. Resolved through the
/// de-facto-standard xterm palette, the slots land like this (contrast against
/// white / against black, for the plain slot and for its bright variant):
///
/// | slot    | L      | plain·white | plain·black | bright·white | bright·black |
/// |---------|--------|-------------|-------------|--------------|--------------|
/// | Red     | 0.1298 |     5.84    |     3.60    |     4.00     |     5.25     |
/// | Green   | 0.4366 |     2.16    |     9.73    |     1.37     |    15.30     |
/// | Yellow  | 0.5664 |     1.70    |    12.33    |     1.07     |    19.56     |
/// | Blue    | 0.0617 |     9.40    |     2.23    |     4.74     |     4.43     |
/// | Magenta | 0.1739 |     4.69    |     4.48    |     3.14     |     6.70     |
/// | Cyan    | 0.4807 |     1.98    |    10.61    |     1.25     |    16.75     |
///
/// Magenta's luminance is within 3% of the theoretical optimum — no other free
/// slot is close. Terminals ship a background and a palette *together*, and the
/// palette is tuned to that background: a light profile darkens its colours, a
/// dark profile brightens them, which is what the bright half of the sixteen
/// exists for. On such a theme-matched terminal magenta clears AA on both —
/// **4.69:1** plain-on-white, **6.70:1** bright-on-black. In the mismatched
/// configurations, including the bold-as-bright hazard that produced the
/// 1.07:1 measurement above, it still holds **3.14:1** and **4.48:1**, above
/// the 3:1 WCAG SC 1.4.11 floor for non-text UI components (a border glyph and
/// a status chip are exactly that).
///
/// Red is the only other slot that clears the same two bars, and it is
/// [`STATUS_ERROR`]. So the choice is forced rather than aesthetic — and the
/// role stays distinct from every other one: green/blue/red/dark-gray
/// statuses, cyan focus, `Reset` selection.
///
/// The numbers above are not prose: `theme/contrast/002` recomputes them from
/// whatever this constant currently is, so a regression to an unreadable slot
/// fails the fast tier instead of shipping.
pub const STATUS_WAITING: Color = Color::Magenta;
/// Error — the agent hit a failure.
pub const STATUS_ERROR: Color = Color::Red;
/// Idle — no current activity (dimmed).
pub const STATUS_IDLE: Color = Color::DarkGray;

// ---------------------------------------------------------------------------
// Accent roles (must be distinct from every status color and from each other)
// ---------------------------------------------------------------------------

/// The focused embedded pane. Cyan was originally used for focus *and*
/// selection; PRD #155 Option A split them so the two are provably distinct.
pub const FOCUSED: Color = Color::Cyan;

/// The selected deck card's border (paired with `BorderType::Thick` and the
/// `▸ ` title marker — see `ui::card_border_glyph`).
///
/// This is [`Color::Reset`] — the **terminal's own default foreground** — and
/// that is the entire point. It is not "no colour": a terminal draws its default
/// foreground near-white on a dark theme and near-black on a light one, so this
/// single role is high-contrast against the user's background on BOTH themes
/// without the TUI ever detecting which one is in use. It cannot collide with
/// anything either, because every other role here is a named hue.
///
/// ## Why not a hue (issue #442)
///
/// Selection was Magenta, dimmed in `UiMode::PaneInput` — and dimmed magenta on
/// a dark background sits in the same band as [`STATUS_IDLE`], so the selected
/// card read as an idle one. The first fix moved selection off colour entirely
/// and onto border thickness alone. That cured the fading but left a second
/// hole: a selected IDLE card then drew a THICK border in [`STATUS_IDLE`]
/// DarkGray, and thickening a line that is nearly the colour of the background
/// does not make it any easier to see. The missing property was contrast, not
/// weight — so selection now carries both.
///
/// Hardcoding `Color::White` would reintroduce the same bug mirrored: a white
/// border is invisible on a light theme exactly as DarkGray is on a dark one.
/// `Color::Reset` delegates the choice to the terminal, the only party that
/// knows. This is the same reasoning behind PRD #13's ban on absolute colours.
///
/// The cost, accepted knowingly: a selected card's BORDER no longer reports
/// status. Its status badge still does, at full colour, in the top-right corner.
pub const SELECTED: Color = Color::Reset;

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
        SessionStatus::Idle => STATUS_IDLE,
        // PRD #162 forward-compat: an unknown wire status renders with the
        // neutral idle color so it never masquerades as an active state.
        SessionStatus::Unknown => STATUS_IDLE,
    }
}

/// This status's rank in the PRD #333 fixed priority order — lower ranks
/// win. Mirrors the aliasing [`status_color`] already applies (Compacting
/// shares Thinking's rank, Unknown shares Idle's) so a status that resolves
/// to the same color also resolves to the same priority.
fn priority_rank(status: &SessionStatus) -> u8 {
    match status {
        SessionStatus::Error => 0,
        SessionStatus::WaitingForInput => 1,
        SessionStatus::Working => 2,
        SessionStatus::Thinking | SessionStatus::Compacting => 3,
        SessionStatus::Idle | SessionStatus::Unknown => 4,
    }
}

/// PRD #333 M1 — resolve the single highest-priority `SessionStatus` among an
/// orchestration tab's pane statuses, per the fixed order Error > NeedsInput >
/// Working > Thinking > Idle (ties within a rank keep whichever status was
/// encountered first). An empty slice (a tab with no panes) falls back to
/// `Idle`, the same neutral "nothing going on" state an all-Idle tab
/// resolves to.
pub fn highest_priority_status(statuses: &[SessionStatus]) -> SessionStatus {
    statuses
        .iter()
        .min_by_key(|status| priority_rank(status))
        .cloned()
        .unwrap_or(SessionStatus::Idle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: feed `highest_priority_status` slices covering every rank in
    /// the PRD #333 table (including the Compacting→Thinking and
    /// Unknown→Idle aliases) and confirm it always returns the single
    /// highest-priority status present, plus the defined no-panes fallback.
    #[test]
    fn highest_priority_status_orders_by_priority() {
        use SessionStatus::*;

        assert_eq!(highest_priority_status(&[Idle, Error, Idle]), Error);
        assert_eq!(
            highest_priority_status(&[Working, WaitingForInput, Idle]),
            WaitingForInput
        );
        assert_eq!(highest_priority_status(&[Idle, Idle, Idle]), Idle);
        assert_eq!(highest_priority_status(&[Thinking, Working]), Working);
        assert_eq!(highest_priority_status(&[Compacting, Idle]), Compacting);
        assert_eq!(highest_priority_status(&[Unknown, Idle]), Unknown);
        assert_eq!(highest_priority_status(&[Error, WaitingForInput]), Error);
        assert_eq!(highest_priority_status(&[]), Idle);
    }
}
