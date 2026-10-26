//! PRD #1666 part 3, round 5-wake: what an agent's screen shows, read
//! losslessly, and the measured recognisers the doorbell's admission asks.
//!
//! [`crate::pane_screen_text`] reduces a screen to trimmed, collapsed text rows,
//! which is right for a notice and wrong for a proof: it drops the dim
//! attribute that separates Codex's placeholder from typed text, collapses the
//! spaces inside a line, and loses where a row's text ends. [`Grid`] keeps every
//! cell of the screen with the attributes the profiles read, each row's
//! soft-wrap flag, the cursor, and the stream position it was taken at.
//!
//! The recognisers below each answer one question about one agent's screen,
//! from shapes measured on real launches (M1, M2): the empty ready composer,
//! the placeholder an agent draws while its input is empty, the deck's
//! own line held exactly in the composer, the dialogs that can open after the
//! agent is ready, and the rows that tie a redraw to the terminal width. None
//! of them authenticates a screen: a process that controls what the pane
//! draws can draw any of these frames.

/// One cell of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridCell {
    /// The cell's text; empty for a cell nothing was written to, and for the
    /// second half of a wide character.
    pub text: String,
    pub bold: bool,
    pub dim: bool,
    pub inverse: bool,
    pub fg: vt100::Color,
    pub bg: vt100::Color,
}

impl GridCell {
    /// The cell's text, with an empty cell read as a blank.
    pub fn shown(&self) -> &str {
        if self.text.is_empty() {
            " "
        } else {
            &self.text
        }
    }

    /// Whether nothing visible is drawn in the cell.
    pub fn blank(&self) -> bool {
        self.text.trim().is_empty() && self.bg == vt100::Color::Default && !self.inverse
    }
}

/// A screen, cell by cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Vec<GridCell>>,
    /// Whether each row continues on the next (the agent's text ran past the
    /// right edge rather than ending with a line feed).
    pub wrapped: Vec<bool>,
    /// The cursor's row and column.
    pub cursor: (u16, u16),
    pub cursor_hidden: bool,
    pub alternate_screen: bool,
    /// How many bytes of the stream the screen was parsed from.
    pub out_seq: u64,
}

impl Grid {
    /// Read `screen` cell by cell.
    pub fn of(screen: &vt100::Screen, out_seq: u64) -> Self {
        let (rows, cols) = crate::vt_grid::size(screen);
        let mut cells = Vec::with_capacity(rows as usize);
        let mut wrapped = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let mut row = Vec::with_capacity(cols as usize);
            for c in 0..cols {
                let cell = screen.cell(r, c);
                row.push(match cell {
                    Some(cell) => GridCell {
                        text: if cell.is_wide_continuation() {
                            String::new()
                        } else {
                            cell.contents().to_string()
                        },
                        bold: cell.bold(),
                        dim: cell.dim(),
                        inverse: cell.inverse(),
                        fg: cell.fgcolor(),
                        bg: cell.bgcolor(),
                    },
                    None => GridCell {
                        text: String::new(),
                        bold: false,
                        dim: false,
                        inverse: false,
                        fg: vt100::Color::Default,
                        bg: vt100::Color::Default,
                    },
                });
            }
            cells.push(row);
            wrapped.push(screen.row_wrapped(r));
        }
        Self {
            rows,
            cols,
            cells,
            wrapped,
            cursor: screen.cursor_position(),
            cursor_hidden: screen.hide_cursor(),
            alternate_screen: screen.alternate_screen(),
            out_seq,
        }
    }

    /// Parse `bytes` from a fresh terminal at `rows` × `cols` and read the
    /// result: how a recorded trace becomes a fixture screen.
    pub fn parse(bytes: &[u8], rows: u16, cols: u16) -> Self {
        let mut parser = crate::vt_grid::parser(rows, cols, 0);
        parser.process(bytes);
        Self::of(parser.screen(), bytes.len() as u64)
    }

    pub fn cell(&self, row: u16, col: u16) -> Option<&GridCell> {
        self.cells.get(row as usize)?.get(col as usize)
    }

    /// Row `row` as drawn, every cell included: untouched cells as blanks,
    /// nothing trimmed.
    pub fn row_text(&self, row: u16) -> String {
        self.cells
            .get(row as usize)
            .map(|cells| cells.iter().map(GridCell::shown).collect())
            .unwrap_or_default()
    }

    /// The text of `len` cells of `row` from `col`.
    pub fn span_text(&self, row: u16, col: u16, len: u16) -> String {
        (col..col.saturating_add(len))
            .filter_map(|c| self.cell(row, c))
            .map(GridCell::shown)
            .collect()
    }

    /// The column just past the last non-blank cell of `row` (0 for a blank
    /// row).
    pub fn row_extent(&self, row: u16) -> u16 {
        self.cells
            .get(row as usize)
            .and_then(|cells| cells.iter().rposition(|c| !c.blank()))
            .map(|i| i as u16 + 1)
            .unwrap_or(0)
    }

    /// Whether every cell of `row` from `col` on is blank.
    pub fn blank_from(&self, row: u16, col: u16) -> bool {
        self.cells
            .get(row as usize)
            .is_some_and(|cells| cells.iter().skip(col as usize).all(GridCell::blank))
    }

    /// The last row with anything visible on it.
    pub fn last_drawn_row(&self) -> Option<u16> {
        (0..self.rows).rev().find(|&r| self.row_extent(r) > 0)
    }
}

impl Grid {
    /// The column just past the last cell anything was written to on `row`,
    /// written blanks included: a row an agent pads to the edge reaches
    /// [`Grid::cols`], while erased cells hold nothing.
    pub fn written_extent(&self, row: u16) -> u16 {
        self.cells
            .get(row as usize)
            .and_then(|cells| cells.iter().rposition(|c| !c.text.is_empty()))
            .map(|i| i as u16 + 1)
            .unwrap_or(0)
    }

    /// The text of `row` from `col`, trailing blanks removed.
    pub fn text_from(&self, row: u16, col: u16) -> String {
        let text: String = self
            .cells
            .get(row as usize)
            .map(|cells| {
                cells
                    .iter()
                    .skip(col as usize)
                    .map(GridCell::shown)
                    .collect()
            })
            .unwrap_or_default();
        text.trim_end().to_string()
    }
}

/// A dialog a recogniser found on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialog {
    /// One of the release's measured post-ready dialogs, by the name the
    /// key-effect measurements use (`update`, `trust`, `hooks`): the markers
    /// an inherited release's tripwire is to be set by (plan §4.6).
    Measured(&'static str),
    /// A screen with the dialog shape the profile knows (a highlighted
    /// numbered choice) but a title it does not.
    Unrecognised,
}

/// What the composer holds, against the line the deck typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerText {
    /// Nothing typed: blank, or the agent's placeholder (`placeholder`).
    Empty { placeholder: bool },
    /// Exactly the line, with the cursor right after it.
    Exact,
    /// A strict prefix of the line, cursor at its end: still arriving.
    Prefix { chars: usize },
    /// Anything else, the line with the cursor elsewhere included.
    Other,
}

/// The composer a profile located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composer {
    /// The row holding the prompt glyph.
    pub prompt_row: u16,
    /// The last row the composer's text reaches.
    pub last_row: u16,
    /// The composer's text, its rows joined by the profile's wrap rule.
    pub text: String,
    /// Every visible cell of the text is drawn dim (the placeholder style).
    pub dim: bool,
    /// The cursor sits right after the text.
    pub cursor_at_end: bool,
}

/// The measured screen of one agent, across the releases its record lists.
pub struct ScreenProfile {
    pub id: &'static str,
    /// The composer, wherever it is drawn, or `None` when the screen shows
    /// none (a dialog, another screen).
    pub composer: fn(&Grid) -> Option<Composer>,
    /// The ready screen: the composer, plus whatever the agent draws only
    /// while it waits for input.
    pub ready: fn(&Grid) -> bool,
    /// Whether a dim text in the composer is the agent's placeholder, which it
    /// showed while its input was empty and never over typed text in the
    /// measurements.
    pub placeholder: fn(&str) -> bool,
    pub dialog: fn(&Grid) -> Option<Dialog>,
    /// Whether the transcript shows `line` as a submitted user message.
    pub transcript_user_line: fn(&Grid, &str) -> bool,
    /// The bytes the agent writes first when it redraws its whole screen
    /// after a resize.
    pub redraw_marker: &'static [u8],
    /// Whether a frame, read straight off the parser's screen, ties itself to
    /// the terminal's width, given every width the terminal has had.
    pub width_witness: fn(&vt100::Screen, &WidthsSeen) -> bool,
    /// The narrowest pane the measurements cover; a narrower one refuses.
    pub min_cols: u16,
}

impl ScreenProfile {
    /// The composer's content against `line`.
    pub fn holds(&self, grid: &Grid, line: &str) -> Option<ComposerText> {
        let composer = (self.composer)(grid)?;
        if composer.text.is_empty() || (composer.dim && (self.placeholder)(&composer.text)) {
            return Some(ComposerText::Empty {
                placeholder: !composer.text.is_empty(),
            });
        }
        if composer.dim {
            return Some(ComposerText::Other);
        }
        if !composer.cursor_at_end {
            return Some(ComposerText::Other);
        }
        if composer.text == line {
            return Some(ComposerText::Exact);
        }
        if line.starts_with(&composer.text) {
            return Some(ComposerText::Prefix {
                chars: composer.text.chars().count(),
            });
        }
        Some(ComposerText::Other)
    }

    /// The empty ready composer, with no dialog over it: what the doorbell
    /// needs before it types (step 2). A blank one only with the cursor where
    /// an empty composer keeps it, as [`crate::doorbell_txn::Frame`] reads it.
    pub fn empty_and_ready(&self, grid: &Grid) -> bool {
        grid.cols >= self.min_cols
            && (self.dialog)(grid).is_none()
            && (self.ready)(grid)
            && match self.holds(grid, "\u{0}") {
                Some(ComposerText::Empty { placeholder: true }) => true,
                Some(ComposerText::Empty { placeholder: false }) => {
                    (self.composer)(grid).is_some_and(|c| c.cursor_at_end)
                }
                _ => false,
            }
    }

    /// The agent's placeholder in the composer: drawn while its input is
    /// empty, so it confirms a withdrawal where a blank composer cannot.
    pub fn placeholder_empty(&self, grid: &Grid) -> bool {
        matches!(
            self.holds(grid, "\u{0}"),
            Some(ComposerText::Empty { placeholder: true })
        )
    }
}

/// The widths a generation's terminal has had (plan §2.2): an agent can only
/// draw for a width its terminal reported, so a witness that two widths draw
/// alike can still tell them apart while one of them never occurred.
#[derive(Clone, PartialEq, Eq)]
pub struct WidthsSeen {
    bits: Box<[u64; 1024]>,
}

impl std::fmt::Debug for WidthsSeen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let widths: Vec<u16> = (0..=u16::MAX).filter(|&w| self.had(w)).collect();
        f.debug_tuple("WidthsSeen").field(&widths).finish()
    }
}

impl WidthsSeen {
    /// Every width in `widths`.
    pub fn of(widths: impl IntoIterator<Item = u16>) -> Self {
        let mut seen = Self {
            bits: Box::new([0; 1024]),
        };
        for cols in widths {
            seen.record(cols);
        }
        seen
    }

    pub fn record(&mut self, cols: u16) {
        self.bits[cols as usize / 64] |= 1 << (cols % 64);
    }

    pub fn had(&self, cols: u16) -> bool {
        self.bits[cols as usize / 64] & (1 << (cols % 64)) != 0
    }
}

/// Whether row `row` of `screen` was drawn to the right edge as one padded
/// row for this width, and ended there: its last cell holds a written blank
/// (padding, which an agent writes up to the width it believes in), the row
/// did not run on to the next one, it is not itself the run-on of the row
/// above, and the stream has moved the cursor off it ([`row_ended`]). A row
/// padded for a wider terminal runs past this edge, so the parser (which
/// always wraps at the edge) marks it wrapped once the cell past the edge
/// arrives; one padded for a narrower terminal stops short of the edge.
///
/// This holds for a row the agent draws in one run, as every witness row in
/// the measured redraws was, however the PTY splits its reads. A row drawn in
/// pieces with a cursor move between them, the first piece ending exactly at
/// the new edge, is not always told apart: a move off the row, or back along
/// the same row away from its last cell, reads as the row's end, and a move
/// the parser clamps to the last cell lets a one-cell next piece overwrite
/// that cell instead of running past it.
fn padded_edge_row(screen: &vt100::Screen, row: u16) -> bool {
    let (_, cols) = crate::vt_grid::size(screen);
    let padded = screen
        .cell(row, cols - 1)
        .is_some_and(|c| c.has_contents() && c.contents().trim().is_empty());
    padded
        && !screen.row_wrapped(row)
        && (row == 0 || !screen.row_wrapped(row - 1))
        && row_ended(screen, row)
}

/// Whether the stream shows that row `row` ended at this width: the cursor is
/// off the row's last cell. Writing that cell leaves the parser's cursor on
/// it, waiting to wrap, so a PTY read that ends there has delivered a row
/// that a wider redraw continues past the edge in the next read; a carriage
/// return, a line feed, a cursor move or the next row drawn takes the cursor
/// away and ends the row. Until one arrives the row is not a witness, however
/// the screen looks.
fn row_ended(screen: &vt100::Screen, row: u16) -> bool {
    let (_, cols) = crate::vt_grid::size(screen);
    let (cursor_row, cursor_col) = screen.cursor_position();
    cursor_row != row || cursor_col < cols - 1
}

/// Whether row `row` of `screen` holds `N.` from column `col`.
fn screen_numbered(screen: &vt100::Screen, row: u16, col: u16) -> bool {
    let (_, cols) = crate::vt_grid::size(screen);
    let mut digits = 0;
    for c in col..cols {
        let text = screen.cell(row, c).map(vt100::Cell::contents).unwrap_or("");
        if text.len() == 1 && text.as_bytes()[0].is_ascii_digit() {
            digits += 1;
            continue;
        }
        return digits > 0 && text == ".";
    }
    false
}

/// Whether row `row` of `screen` reads `prefix` from column `col`, one
/// character per cell.
fn screen_text_starts(screen: &vt100::Screen, row: u16, col: u16, prefix: &str) -> bool {
    prefix.chars().enumerate().all(|(i, ch)| {
        screen.cell(row, col + i as u16).is_some_and(|c| {
            let mut text = c.contents().chars();
            text.next() == Some(ch) && text.next().is_none()
        })
    })
}

/// Where the first `needle` sits in `bytes`.
pub fn find_marker(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    bytes.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Codex (measured on 0.160.0 and 0.164.0, M2, 2026-10-24)
// ---------------------------------------------------------------------------

/// Codex's composer indent: text starts two cells in, after `› `.
const CODEX_INDENT: u16 = 2;

/// The texts Codex shows, dim, in an empty composer.
fn codex_placeholder(text: &str) -> bool {
    matches!(
        text,
        "Ask Codex to do anything" | "Ask a follow-up question"
    )
}

fn codex_glyph(grid: &Grid, row: u16) -> Option<&GridCell> {
    grid.cell(row, 0).filter(|c| c.text == "›")
}

/// A numbered choice after the glyph: `› 1. …`.
fn codex_numbered(grid: &Grid, row: u16) -> bool {
    let text = grid.text_from(row, CODEX_INDENT);
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && text[digits..].starts_with('.')
}

/// The composer: the lowest row whose first cell is a bold `›` that is
/// neither dim (a submitted message in the transcript, whose glyph Codex
/// draws dim) nor inverse (a dialog's highlighted choice), with its text from column 2 and the rows
/// below it at the same indent until a blank row, joined at single spaces
/// (Codex wraps the line at word boundaries).
fn codex_composer(grid: &Grid) -> Option<Composer> {
    let prompt_row = (0..grid.rows).rev().find(|&r| {
        codex_glyph(grid, r).is_some_and(|c| c.bold && !c.dim && !c.inverse)
            && !codex_numbered(grid, r)
    })?;
    let mut text = grid.text_from(prompt_row, CODEX_INDENT);
    let mut last_row = prompt_row;
    let mut row = prompt_row + 1;
    while row < grid.rows {
        let next = grid.text_from(row, CODEX_INDENT);
        if next.is_empty() || !grid.text_from(row, 0).starts_with("  ") {
            break;
        }
        // A row below the composer that is not its continuation: the model
        // line and the footer sit after a blank row.
        text.push(' ');
        text.push_str(next.trim_start());
        last_row = row;
        row += 1;
    }
    let mut dim = true;
    let mut any = false;
    for r in prompt_row..=last_row {
        let from = if r == prompt_row { CODEX_INDENT } else { 0 };
        for cell in grid.cells[r as usize].iter().skip(from as usize) {
            if !cell.text.trim().is_empty() {
                any = true;
                dim &= cell.dim;
            }
        }
    }
    let end_col = if last_row == prompt_row {
        CODEX_INDENT + grid.text_from(prompt_row, CODEX_INDENT).chars().count() as u16
    } else {
        grid.text_from(last_row, 0).chars().count() as u16
    };
    let cursor_at_end = if text.is_empty() || (any && dim) {
        grid.cursor == (prompt_row, CODEX_INDENT)
    } else {
        grid.cursor == (last_row, end_col)
    };
    Some(Composer {
        prompt_row,
        last_row,
        text,
        dim: any && dim,
        cursor_at_end,
    })
}

/// The ready screen: the composer, and `? for shortcuts` on the last drawn
/// row, which Codex hid while its composer held text in the measurements.
fn codex_ready(grid: &Grid) -> bool {
    let Some(composer) = codex_composer(grid) else {
        return false;
    };
    let Some(footer) = grid.last_drawn_row() else {
        return false;
    };
    let footer_text = grid.text_from(footer, 0);
    let footer_text = footer_text.trim_start();
    footer > composer.last_row
        && (footer_text.starts_with("? for shortcuts") || footer_text.contains("· ? for shortcuts"))
}

/// Codex's dialogs (update, folder trust, hooks review, measured): a bold
/// title at row 1, column 2, and a highlighted choice — `›` at column 0,
/// bold and inverse, then `N.` — drawn to the right edge. Text that merely
/// mentions a dialog's words in the transcript has neither.
fn codex_dialog(grid: &Grid) -> Option<Dialog> {
    let choice = (0..grid.rows).any(|r| {
        codex_glyph(grid, r).is_some_and(|c| c.inverse && c.bold)
            && codex_numbered(grid, r)
            && grid.written_extent(r) == grid.cols
    });
    if !choice {
        return None;
    }
    let title_bold = grid.cell(1, 2).is_some_and(|c| c.bold);
    let title = grid.text_from(1, 2);
    Some(match title.as_str() {
        t if title_bold && t.starts_with("Update available") => Dialog::Measured("update"),
        "Folder access" if title_bold => Dialog::Measured("trust"),
        "Hooks need review" if title_bold => Dialog::Measured("hooks"),
        _ => Dialog::Unrecognised,
    })
}

/// A submitted user message in Codex's transcript: `›` at column 0 drawn
/// dim (Codex writes it bold then dim, which leaves it dim; the composer's
/// glyph is bold and not dim), its text from column 2 with continuation
/// rows at the same indent, above the composer.
fn codex_transcript_user_line(grid: &Grid, line: &str) -> bool {
    let composer_row = codex_composer(grid)
        .map(|c| c.prompt_row)
        .unwrap_or(grid.rows);
    (0..composer_row).any(|r| {
        if !codex_glyph(grid, r).is_some_and(|c| c.dim && !c.inverse) {
            return false;
        }
        let mut text = grid.text_from(r, CODEX_INDENT);
        let mut row = r + 1;
        while row < composer_row {
            let next = grid.text_from(row, 0);
            if next.trim().is_empty() || !next.starts_with("  ") {
                break;
            }
            text.push(' ');
            text.push_str(next.trim());
            row += 1;
        }
        text == line
    })
}

/// Whether a frame ties itself to the width it was drawn at (`seen`: every
/// width the generation's terminal has had). Either a padded edge row
/// ([`padded_edge_row`]) of a shape Codex pads to the edge: a dialog's
/// highlighted choice (`›` bold and inverse, then `N.`), or the dim
/// `Worked for …` row under a finished turn, every cell dim. Or, before the
/// first prompt, Codex's logo centred with its top row starting at column
/// `cols / 2 - 5` (measured at eleven widths from 90 to 200 on 0.160.0 and
/// 0.164.0); that formula puts the logo of the two widths `2k` and `2k + 1`
/// in the same place, so the logo counts only while the terminal has never
/// had the other one of the pair.
fn codex_width_witness(screen: &vt100::Screen, seen: &WidthsSeen) -> bool {
    let (rows, cols) = crate::vt_grid::size(screen);
    let edge_row = (0..rows).any(|r| {
        padded_edge_row(screen, r) && {
            let glyph = screen.cell(r, 0);
            let choice = glyph.is_some_and(|c| c.contents() == "›" && c.bold() && c.inverse())
                && screen_numbered(screen, r, CODEX_INDENT);
            let turn = screen_text_starts(screen, r, CODEX_INDENT, "Worked for ")
                && (0..cols).all(|c| screen.cell(r, c).is_some_and(vt100::Cell::dim));
            choice || turn
        }
    });
    if edge_row {
        return true;
    }
    let braille = |r: u16, c: u16| {
        screen.cell(r, c).is_some_and(|cell| {
            cell.contents()
                .chars()
                .next()
                .is_some_and(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
        })
    };
    let Some((_, first)) =
        (0..rows).find_map(|r| (0..cols).find(|&c| braille(r, c)).map(|c| (r, c)))
    else {
        return false;
    };
    (cols / 2).checked_sub(5) == Some(first) && !seen.had(cols ^ 1)
}

/// Codex 0.160.0 and 0.164.0: alternate screen; after a resize Codex waits
/// about two seconds, then redraws the whole screen at the new width
/// (`ESC[?2026h ESC[1;1H ESC[J`; after each of 10 resizes and 10 nudges on
/// each release, before and after a first prompt).
pub static CODEX_PROFILE: ScreenProfile = ScreenProfile {
    id: "codex-2026-10",
    composer: codex_composer,
    ready: codex_ready,
    placeholder: codex_placeholder,
    dialog: codex_dialog,
    transcript_user_line: codex_transcript_user_line,
    redraw_marker: b"\x1b[?2026h\x1b[1;1H\x1b[J",
    width_witness: codex_width_witness,
    min_cols: 80,
};

// ---------------------------------------------------------------------------
// Claude Code (measured on 2.1.310, M1, 2026-10-24)
// ---------------------------------------------------------------------------

/// Claude Code's composer indent: text starts two cells in, after `❯` and a
/// no-break space.
const CLAUDE_INDENT: u16 = 2;

/// A rule Claude Code draws across the whole width above and below its
/// composer.
fn claude_rule(grid: &Grid, row: u16) -> bool {
    grid.written_extent(row) == grid.cols
        && grid
            .cells
            .get(row as usize)
            .is_some_and(|cells| cells.iter().all(|c| c.text == "─"))
}

/// The composer: `❯` at column 0 on the row below a full-width rule, its text
/// from column 2 and the rows below it at the same indent up to the next
/// full-width rule, joined at single spaces (Claude Code wraps at word
/// boundaries). A user message in the transcript is painted on a background
/// colour; the composer's glyph is not.
fn claude_composer(grid: &Grid) -> Option<Composer> {
    let prompt_row = (1..grid.rows).rev().find(|&r| {
        grid.cell(r, 0)
            .is_some_and(|c| c.text == "❯" && c.bg == vt100::Color::Default)
            && claude_rule(grid, r - 1)
    })?;
    let close = (prompt_row + 1..grid.rows).find(|&r| claude_rule(grid, r))?;
    let mut text = grid.text_from(prompt_row, CLAUDE_INDENT);
    let mut last_row = prompt_row;
    for row in prompt_row + 1..close {
        let next = grid.text_from(row, 0);
        if next.trim().is_empty() {
            break;
        }
        text.push(' ');
        text.push_str(next.trim());
        last_row = row;
    }
    let mut dim = true;
    let mut any = false;
    for r in prompt_row..=last_row {
        let from = if r == prompt_row { CLAUDE_INDENT } else { 0 };
        for cell in grid.cells[r as usize].iter().skip(from as usize) {
            if !cell.text.trim().is_empty() {
                any = true;
                dim &= cell.dim;
            }
        }
    }
    let end_col = if last_row == prompt_row {
        CLAUDE_INDENT + grid.text_from(prompt_row, CLAUDE_INDENT).chars().count() as u16
    } else {
        grid.text_from(last_row, 0).chars().count() as u16
    };
    let cursor_at_end = if text.is_empty() || (any && dim) {
        grid.cursor == (prompt_row, CLAUDE_INDENT)
    } else {
        grid.cursor == (last_row, end_col)
    };
    Some(Composer {
        prompt_row,
        last_row,
        text,
        dim: any && dim,
        cursor_at_end,
    })
}

/// The ready screen: the composer framed by two full-width rules with a
/// status row under the lower one.
fn claude_ready(grid: &Grid) -> bool {
    let Some(composer) = claude_composer(grid) else {
        return false;
    };
    let Some(close) = (composer.last_row + 1..grid.rows).find(|&r| claude_rule(grid, r)) else {
        return false;
    };
    (close + 1..grid.rows).any(|r| grid.row_extent(r) > 0)
}

/// Claude Code's dialogs show their choices as `❯ 1. …`; none was seen after
/// readiness in M1's launches, so any such screen is unrecognised.
fn claude_dialog(grid: &Grid) -> Option<Dialog> {
    (0..grid.rows)
        .any(|r| {
            let text = grid.text_from(r, 0);
            let text = text.trim_start();
            text.strip_prefix('❯').is_some_and(|rest| {
                let rest = rest.trim_start_matches([' ', '\u{a0}']);
                let digits = rest.chars().take_while(char::is_ascii_digit).count();
                digits > 0 && rest[digits..].starts_with('.')
            })
        })
        .then_some(Dialog::Unrecognised)
}

/// A user message in Claude Code's transcript: `❯` at column 0 on a row
/// painted background colour 237, its text from column 2 with continuation
/// rows on the same background.
fn claude_transcript_user_line(grid: &Grid, line: &str) -> bool {
    let user_bg = vt100::Color::Idx(237);
    (0..grid.rows).any(|r| {
        if !grid
            .cell(r, 0)
            .is_some_and(|c| c.text == "❯" && c.bg == user_bg)
        {
            return false;
        }
        let mut text = grid.text_from(r, CLAUDE_INDENT);
        let mut row = r + 1;
        while row < grid.rows && grid.cell(row, 0).is_some_and(|c| c.bg == user_bg) {
            let next = grid.text_from(row, 0);
            if next.trim().is_empty() {
                break;
            }
            text.push(' ');
            text.push_str(next.trim());
            row += 1;
        }
        text == line
    })
}

/// A full-width rule tied to the width the agent drew at: every cell `─`, the
/// row neither running on to the next one nor the run-on of the row above,
/// and the cursor moved off it ([`row_ended`]). A rule drawn for a wider
/// terminal runs past this edge once its next cell arrives, which the parser
/// (always wrapping at the edge) marks; one drawn for a narrower terminal
/// stops short of it. Like Codex's rows, this holds for a rule drawn in one
/// run, as every measured redraw drew it ([`padded_edge_row`] says what a
/// rule drawn in two pieces does).
fn claude_width_witness(screen: &vt100::Screen, _seen: &WidthsSeen) -> bool {
    let (rows, cols) = crate::vt_grid::size(screen);
    (0..rows).any(|r| {
        (0..cols).all(|c| screen.cell(r, c).is_some_and(|cell| cell.contents() == "─"))
            && !screen.row_wrapped(r)
            && (r == 0 || !screen.row_wrapped(r - 1))
            && row_ended(screen, r)
    })
}

/// The suggestion Claude Code shows, dim, in an empty composer: `Try "…"`,
/// with a different suggestion on each launch.
fn claude_placeholder(text: &str) -> bool {
    text.starts_with("Try \"") && text.ends_with('"')
}

/// Claude Code 2.1.310: alternate screen, a full redraw (`ESC[2J ESC[H`)
/// within 9 ms of each of the 10 resizes and 10 nudges measured.
pub static CLAUDE_PROFILE: ScreenProfile = ScreenProfile {
    id: "claude-code-2026-10",
    composer: claude_composer,
    ready: claude_ready,
    placeholder: claude_placeholder,
    dialog: claude_dialog,
    transcript_user_line: claude_transcript_user_line,
    redraw_marker: b"\x1b[2J\x1b[H",
    width_witness: claude_width_witness,
    min_cols: 80,
};

// ---------------------------------------------------------------------------
// The e2e build's stand-in (`tests/support/empty_composer.py`)
// ---------------------------------------------------------------------------

/// The variable that selects the stand-in profile for a pane, set in the
/// pane's own environment: `standin`.
#[cfg(feature = "e2e")]
pub const STANDIN_SELECTOR_ENV: &str = "DOT_AGENT_DECK_TEST_COMPOSER_PROFILE";

/// The variable naming the stand-in's executable, by absolute path: the
/// profile applies only to a pane that runs that file.
#[cfg(feature = "e2e")]
pub const STANDIN_BINARY_ENV: &str = "DOT_AGENT_DECK_TEST_COMPOSER_BINARY";

#[cfg(feature = "e2e")]
const STANDIN_READY: &str = "DOORBELL-STANDIN-READY";
#[cfg(feature = "e2e")]
const STANDIN_COMPOSER: &str = "COMPOSER: ";
#[cfg(feature = "e2e")]
const STANDIN_SUBMITTED: &str = "SUBMITTED:";

/// The stand-in's composer: the row that starts `COMPOSER: `, and the rows
/// it wraps onto when the line is wider than the terminal.
#[cfg(feature = "e2e")]
fn standin_composer(grid: &Grid) -> Option<Composer> {
    let indent = STANDIN_COMPOSER.chars().count() as u16;
    let row = (0..grid.rows).find(|&r| {
        grid.text_from(r, 0)
            .starts_with(STANDIN_COMPOSER.trim_end())
    })?;
    let mut last_row = row;
    while grid
        .wrapped
        .get(last_row as usize)
        .copied()
        .unwrap_or(false)
        && last_row + 1 < grid.rows
    {
        last_row += 1;
    }
    let mut text = String::new();
    let mut shown: Vec<&GridCell> = Vec::new();
    for r in row..=last_row {
        let from = if r == row { indent } else { 0 };
        let cells = &grid.cells[r as usize][from as usize..];
        // A wrapped row is drawn to the edge; only the last may end early.
        let piece = if r < last_row {
            cells.iter().map(GridCell::shown).collect::<String>()
        } else {
            grid.text_from(r, from)
        };
        text.push_str(&piece);
        shown.extend(cells.iter().filter(|c| !c.text.trim().is_empty()));
    }
    let dim = !shown.is_empty() && shown.iter().all(|c| c.dim);
    let end_col = if last_row == row {
        indent + text.chars().count() as u16
    } else {
        grid.text_from(last_row, 0).chars().count() as u16
    };
    Some(Composer {
        prompt_row: row,
        last_row,
        cursor_at_end: grid.cursor == (last_row, end_col),
        text,
        dim,
    })
}

#[cfg(feature = "e2e")]
fn standin_ready(grid: &Grid) -> bool {
    grid.text_from(0, 0) == STANDIN_READY && standin_composer(grid).is_some()
}

#[cfg(feature = "e2e")]
fn standin_placeholder(text: &str) -> bool {
    text == "<empty>"
}

#[cfg(feature = "e2e")]
fn standin_dialog(_grid: &Grid) -> Option<Dialog> {
    None
}

/// A `SUBMITTED:` row holding `line`, with the rows it wraps onto when the
/// line is wider than the terminal.
#[cfg(feature = "e2e")]
fn standin_transcript_user_line(grid: &Grid, line: &str) -> bool {
    (0..grid.rows).any(|r| {
        if !grid.text_from(r, 0).starts_with(STANDIN_SUBMITTED) {
            return false;
        }
        let mut text = String::new();
        let mut row = r;
        while grid.wrapped.get(row as usize).copied().unwrap_or(false) && row + 1 < grid.rows {
            text.extend(grid.cells[row as usize].iter().map(GridCell::shown));
            row += 1;
        }
        text.push_str(&grid.text_from(row, 0));
        text.strip_prefix(STANDIN_SUBMITTED)
            .is_some_and(|text| text == line)
    })
}

/// The stand-in draws a row of `W` across the whole width under its ready
/// row, in one run.
#[cfg(feature = "e2e")]
fn standin_width_witness(screen: &vt100::Screen, _seen: &WidthsSeen) -> bool {
    let (rows, cols) = crate::vt_grid::size(screen);
    rows > 1
        && (0..cols).all(|c| screen.cell(1, c).is_some_and(|cell| cell.contents() == "W"))
        && !screen.row_wrapped(1)
        && !screen.row_wrapped(0)
        && row_ended(screen, 1)
}

/// The e2e build's stand-in editor (`tests/support/empty_composer.py`): its
/// fixed rows are this profile's contract, not a recogniser of any agent.
/// It redraws in full (`ESC[2J ESC[H`) on every key and every resize.
#[cfg(feature = "e2e")]
pub static STANDIN_PROFILE: ScreenProfile = ScreenProfile {
    id: "e2e-standin",
    composer: standin_composer,
    ready: standin_ready,
    placeholder: standin_placeholder,
    dialog: standin_dialog,
    transcript_user_line: standin_transcript_user_line,
    redraw_marker: b"\x1b[2J\x1b[H",
    width_witness: standin_width_witness,
    min_cols: 40,
};

/// The stand-in's executable when a pane's spawn selects the stand-in
/// profile: the selector says `standin`, the binary variable names an
/// absolute path, and the pane's command names that path. Each variable is
/// read from the pane's environment, or else from an assignment in the
/// command itself (an orchestration role's pane carries no start request's
/// environment). Anything less selects nothing.
#[cfg(feature = "e2e")]
pub fn standin_selected(
    env: &[(String, String)],
    command: Option<&str>,
) -> Option<std::path::PathBuf> {
    let command = command?;
    let value = |name: &str| -> Option<String> {
        if let Some((_, v)) = env.iter().rev().find(|(k, _)| k == name) {
            return Some(v.clone());
        }
        let at = command.find(&format!("{name}="))? + name.len() + 1;
        let rest = &command[at..];
        let raw = match rest.strip_prefix('\'') {
            Some(quoted) => quoted.split('\'').next()?,
            None => rest.split(|c: char| c.is_whitespace() || c == ';').next()?,
        };
        Some(raw.to_string())
    };
    if value(STANDIN_SELECTOR_ENV).as_deref() != Some("standin") {
        return None;
    }
    let binary = std::path::PathBuf::from(value(STANDIN_BINARY_ENV)?);
    if !binary.is_absolute() || !command.contains(binary.to_str()?) {
        return None;
    }
    Some(binary)
}

/// The stand-in the daemon's own environment selects, for panes a test
/// cannot give the selector to (an agent the TUI spawns from its configured
/// command): the selector says `standin` and the binary variable names an
/// absolute path. Nothing in the pane's command is checked, so a pane in such
/// a daemon types under the stand-in's profile only while its process runs
/// that path ([`standin_running`], asked at every attempt).
#[cfg(feature = "e2e")]
pub fn standin_from_daemon_env() -> Option<std::path::PathBuf> {
    standin_from(|name| std::env::var(name).ok())
}

#[cfg(feature = "e2e")]
fn standin_from(lookup: impl Fn(&str) -> Option<String>) -> Option<std::path::PathBuf> {
    if lookup(STANDIN_SELECTOR_ENV).as_deref() != Some("standin") {
        return None;
    }
    let binary = std::path::PathBuf::from(lookup(STANDIN_BINARY_ENV)?);
    binary.is_absolute().then_some(binary)
}

/// Whether process `pid` runs `binary`: one of its arguments is that path,
/// as a script's interpreter is given it (Linux; elsewhere nothing is read,
/// and the stand-in is not recognised).
#[cfg(feature = "e2e")]
pub fn standin_running(pid: Option<u32>, binary: &std::path::Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Some(pid) = pid else {
            return false;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return false;
        };
        let wanted = binary.as_os_str().as_encoded_bytes();
        cmdline.split(|b| *b == 0).any(|arg| arg == wanted)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, binary);
        false
    }
}

/// The profile a record names.
pub fn profile(id: &str) -> Option<&'static ScreenProfile> {
    [&CODEX_PROFILE, &CLAUDE_PROFILE]
        .into_iter()
        .find(|p| p.id == id)
}

/// What the doorbell's transaction reads of one frame (PRD #1666 part 3,
/// round 5-wake, plan §1.1): `grid`, recognised by `profile`, as a
/// [`crate::doorbell_txn::Frame`]. `clean` says whether the evidence state is
/// Clean and `epoch` its layout epoch; a frame that is not Clean carries no
/// composer and no dialog, so nothing can be decided on it. `line` is the
/// attempt's `L_t`, for the transcript route. The composer is read only at a
/// width the profile's measurements cover, and an empty one only on the
/// profile's ready screen: an empty input row anywhere else is not the
/// composer the doorbell may type into. A composer holding text is read on
/// whatever screen draws it, since typing can hide what the agent draws
/// only while it waits (Codex's footer hint).
pub fn frame(
    profile: &ScreenProfile,
    grid: &Grid,
    epoch: u64,
    clean: bool,
    line: Option<&str>,
) -> crate::doorbell_txn::Frame {
    use crate::doorbell_txn::{ComposerView, Frame};
    let text = (0..grid.rows)
        .map(|r| grid.text_from(r, 0))
        .collect::<Vec<_>>()
        .join("\n");
    if !clean {
        return Frame {
            epoch,
            clean,
            text,
            ..Frame::default()
        };
    }
    let dialog = (profile.dialog)(grid).map(|dialog| match dialog {
        Dialog::Measured(name) => name,
        Dialog::Unrecognised => "unrecognised",
    });
    let composer = (grid.cols >= profile.min_cols)
        .then(|| (profile.composer)(grid))
        .flatten()
        .filter(|composer| {
            let empty =
                composer.text.is_empty() || (composer.dim && (profile.placeholder)(&composer.text));
            !empty || (profile.ready)(grid)
        })
        .map(|composer| {
            let selection = (composer.prompt_row..=composer.last_row).any(|r| {
                grid.cells[r as usize]
                    .iter()
                    .any(|c| c.inverse && !c.text.trim().is_empty())
            });
            ComposerView {
                placeholder: composer.dim && (profile.placeholder)(&composer.text),
                text: composer.text,
                cursor_at_end: composer.cursor_at_end,
                selection,
            }
        });
    let user_cells = line
        .filter(|line| (profile.transcript_user_line)(grid, line))
        .map(|line| vec![line.to_string()])
        .unwrap_or_default();
    Frame {
        epoch,
        clean,
        dialog,
        composer,
        user_cells,
        text,
    }
}

/// Why the strict recogniser refuses to withdraw the deck's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalRefusal {
    /// The pane is narrower than the profile's measurements cover.
    Narrow,
    /// A dialog is on screen: deletes would go to it.
    Dialog,
    /// No composer is on screen.
    NotComposer,
    /// The owned text does not begin with a whole ring token, so the deck
    /// cannot tell its own text from anyone else's.
    NoWholeToken,
    /// The composer holds something other than exactly the owned text.
    NotOwnedText,
    /// The cursor is not right after the owned text.
    CursorElsewhere,
    /// Part of the composer is highlighted (a selection).
    Selection,
}

/// The length of a ring token's visible head, `(` + six letters + `)`.
pub const TOKEN_HEAD_CHARS: usize = 8;

/// Whether the deck may delete `owned`, the text it wrote and the agent
/// accepted (all of its line, or after a partial write exactly its accepted
/// prefix), from this frame: the composer holds exactly `owned`, starting
/// with a whole token, with the cursor right after it, nothing highlighted
/// and no dialog on screen. Returns the number of delete keys to send, one per
/// character. Called on a fresh frame immediately before the deletes; the
/// deletes still go to whatever owns the input when they arrive.
pub fn strict_withdrawal(
    profile: &ScreenProfile,
    grid: &Grid,
    owned: &str,
) -> Result<usize, WithdrawalRefusal> {
    if grid.cols < profile.min_cols {
        return Err(WithdrawalRefusal::Narrow);
    }
    if (profile.dialog)(grid).is_some() {
        return Err(WithdrawalRefusal::Dialog);
    }
    let head: Vec<char> = owned.chars().take(TOKEN_HEAD_CHARS).collect();
    let whole_token = head.len() == TOKEN_HEAD_CHARS
        && head[0] == '('
        && head[TOKEN_HEAD_CHARS - 1] == ')'
        && head[1..TOKEN_HEAD_CHARS - 1]
            .iter()
            .all(char::is_ascii_lowercase);
    if !whole_token {
        return Err(WithdrawalRefusal::NoWholeToken);
    }
    let composer = (profile.composer)(grid).ok_or(WithdrawalRefusal::NotComposer)?;
    if composer.dim || composer.text != owned {
        return Err(WithdrawalRefusal::NotOwnedText);
    }
    if !composer.cursor_at_end {
        return Err(WithdrawalRefusal::CursorElsewhere);
    }
    let highlighted = (composer.prompt_row..=composer.last_row).any(|r| {
        grid.cells[r as usize]
            .iter()
            .any(|c| c.inverse && !c.text.trim().is_empty())
    });
    if highlighted {
        return Err(WithdrawalRefusal::Selection);
    }
    Ok(owned.chars().count())
}

/// The profile measured for an agent type, if any: what a generation's
/// evidence state tracks from its spawn.
pub fn profile_for_agent(
    agent: Option<&crate::event::AgentType>,
) -> Option<&'static ScreenProfile> {
    match agent? {
        crate::event::AgentType::Codex => Some(&CODEX_PROFILE),
        crate::event::AgentType::ClaudeCode => Some(&CLAUDE_PROFILE),
        _ => None,
    }
}

#[cfg(all(test, feature = "e2e"))]
mod standin_tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The bytes `tests/support/empty_composer.py` draws for `content`
    /// (`None`: its dim placeholder) and `transcript`, at `cols`.
    fn standin_frame(cols: usize, transcript: &str, content: Option<&str>) -> Vec<u8> {
        let content = match content {
            Some(text) => text.to_string(),
            None => "\x1b[2m<empty>\x1b[22m".to_string(),
        };
        format!(
            "\x1b[2J\x1b[HDOORBELL-STANDIN-READY\r\n{}\r\n{transcript}\r\nPROCESSED:1\r\nCOMPOSER: {content}",
            "W".repeat(cols)
        )
        .into_bytes()
    }

    #[test]
    fn the_standin_needs_its_selector_and_its_own_executable_path() {
        let binary = "/home/t/doorbell-composer";
        let command = Some("export X=1; exec '/home/t/doorbell-composer' '/home/t/base'");
        let selected = env(&[
            (STANDIN_SELECTOR_ENV, "standin"),
            (STANDIN_BINARY_ENV, binary),
        ]);
        assert_eq!(
            standin_selected(&selected, command),
            Some(std::path::PathBuf::from(binary))
        );
        assert_eq!(
            standin_selected(&env(&[(STANDIN_BINARY_ENV, binary)]), command),
            None,
            "no selector"
        );
        assert_eq!(
            standin_selected(&env(&[(STANDIN_SELECTOR_ENV, "standin")]), command),
            None,
            "a missing path"
        );
        assert_eq!(
            standin_selected(
                &env(&[
                    (STANDIN_SELECTOR_ENV, "standin"),
                    (STANDIN_BINARY_ENV, "/home/t/another-editor"),
                ]),
                command
            ),
            None,
            "a path the pane does not run"
        );
        assert_eq!(
            standin_selected(
                &env(&[
                    (STANDIN_SELECTOR_ENV, "standin"),
                    (STANDIN_BINARY_ENV, "doorbell-composer"),
                ]),
                command
            ),
            None,
            "a relative path"
        );
        assert_eq!(
            standin_selected(
                &env(&[
                    (STANDIN_SELECTOR_ENV, "other"),
                    (STANDIN_BINARY_ENV, binary)
                ]),
                command
            ),
            None,
            "another selector"
        );
    }

    #[test]
    fn the_daemons_environment_selects_the_standin_with_an_absolute_path() {
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            standin_from(lookup(&[
                (STANDIN_SELECTOR_ENV, "standin"),
                (STANDIN_BINARY_ENV, "/home/t/recorder.py"),
            ])),
            Some(std::path::PathBuf::from("/home/t/recorder.py"))
        );
        assert_eq!(
            standin_from(lookup(&[(STANDIN_BINARY_ENV, "/home/t/recorder.py")])),
            None,
            "no selector"
        );
        assert_eq!(
            standin_from(lookup(&[(STANDIN_SELECTOR_ENV, "standin")])),
            None,
            "a missing path"
        );
        assert_eq!(
            standin_from(lookup(&[
                (STANDIN_SELECTOR_ENV, "standin"),
                (STANDIN_BINARY_ENV, "recorder.py"),
            ])),
            None,
            "a relative path"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_standin_is_recognised_only_in_a_process_running_its_path() {
        let own = std::env::current_exe().expect("this test's executable");
        let argv0 = std::env::args().next().expect("argv[0]");
        assert!(standin_running(
            Some(std::process::id()),
            std::path::Path::new(&argv0)
        ));
        assert!(!standin_running(
            Some(std::process::id()),
            &own.with_file_name("not-this-test")
        ));
        assert!(!standin_running(None, &own));
    }

    #[test]
    fn the_standin_profile_reads_its_fixed_rows() {
        let line = "(abcdfg) dot-agent-deck: message m-abcdefgh-0123456789abcdef waiting, run: dot-agent-deck inbox fetch";
        let p = &STANDIN_PROFILE;
        let empty = Grid::parse(&standin_frame(160, "", None), 45, 160);
        assert!(p.empty_and_ready(&empty));
        assert!(p.placeholder_empty(&empty));
        let f = frame(p, &empty, 0, true, Some(line));
        assert!(f.empty_and_ready());
        let typed = Grid::parse(&standin_frame(160, "", Some(line)), 45, 160);
        assert!(frame(p, &typed, 0, true, Some(line)).holds_exactly(line));
        let submitted = Grid::parse(
            &standin_frame(160, &format!("SUBMITTED:{line}"), None),
            45,
            160,
        );
        let f = frame(p, &submitted, 0, true, Some(line));
        assert_eq!(f.user_cells, vec![line.to_string()]);
        assert!(f.empty_and_ready());
        // At a width the submitted row wraps at, it is read through its wrap.
        let narrow = Grid::parse(
            &standin_frame(80, &format!("SUBMITTED:{line}"), None),
            45,
            80,
        );
        let f = frame(p, &narrow, 0, true, Some(line));
        assert_eq!(f.user_cells, vec![line.to_string()]);
        // Its width witness: the W row as wide as the terminal, in one run.
        let mut parser = crate::vt_grid::parser(45, 160, 0);
        parser.process(&standin_frame(160, "", None));
        assert!((p.width_witness)(parser.screen(), &WidthsSeen::of([160])));
        let mut wider = crate::vt_grid::parser(45, 160, 0);
        wider.process(&standin_frame(170, "", None));
        assert!(!(p.width_witness)(wider.screen(), &WidthsSeen::of([160])));
    }
}
