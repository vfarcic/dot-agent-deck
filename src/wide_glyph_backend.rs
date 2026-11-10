//! The deck's ratatui backend: crossterm, with each two-column glyph drawn so
//! it leaves the same screen whether the host terminal gives it one column or
//! two (PRD #385).
//!
//! Host terminals disagree on the width of an emoji-presentation symbol such
//! as `☀️`, `❤️` or `1️⃣` (a symbol followed by VS16, U+FE0F): some draw it two
//! columns wide, as the pane's own screen does, and some draw it one. The
//! deck's frame says it takes two. Drawn as ratatui's crossterm backend draws
//! it, the second column of such a glyph is never written: on a host that
//! draws the glyph two columns wide the glyph covers it, but on one that draws
//! it one column wide whatever that column showed before stays there,
//! highlight included, while a selection copies the new text.
//!
//! [`WideGlyphBackend`] blanks the second column first, then draws the glyph
//! in its first column. On a host that draws the glyph two columns wide the
//! glyph covers the blank; on one that draws it one column wide the second
//! column is left blank, as a fresh render on that host leaves it. An update
//! the frame's diff sends for a column the glyph covers is dropped: that
//! column is the blank just drawn, and the crossterm backend prints an update
//! right after the glyph without moving the cursor, so on a host that draws
//! the glyph two columns wide it would land one column past it. The update
//! after a glyph is therefore at least two columns on, and the crossterm
//! backend moves the cursor to it, whichever width the host gave the glyph.
//! Everything else goes to the wrapped backend unchanged.

use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellDiffOption, CellWidth};
use ratatui::layout::{Position, Size};

/// A ratatui [`Backend`] that draws every two-column glyph so its second column
/// ends up blank or covered by the glyph, whichever width the host terminal
/// gives it. See the module documentation.
#[derive(Debug)]
pub struct WideGlyphBackend<B> {
    inner: B,
    /// The terminal's width as the wrapped backend last reported it, so a
    /// glyph's second column is blanked only where the terminal has one.
    /// `None` until ratatui first asks for the size, which `Terminal::new`
    /// does.
    columns: std::cell::Cell<Option<u16>>,
}

impl<B> WideGlyphBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            columns: std::cell::Cell::new(None),
        }
    }

    pub fn inner(&self) -> &B {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut B {
        &mut self.inner
    }
}

impl<B: Backend> Backend for WideGlyphBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let updates: Vec<(u16, u16, &'a Cell)> = content.collect();
        let columns = self.columns.get();
        // Each wide glyph's blank, made before any update borrows one.
        let blanks: Vec<Cell> = updates
            .iter()
            .filter(|&&(x, _, cell)| blanked_columns(x, cell, columns) > 0)
            .map(|&(_, _, cell)| {
                let mut blank = cell.clone();
                blank.set_symbol(" ").set_diff_option(CellDiffOption::None);
                blank
            })
            .collect();
        let mut blanks = blanks.iter();
        let mut drawn: Vec<(u16, u16, &Cell)> = Vec::with_capacity(updates.len() + blanks.len());
        // The row and the columns `[from, to)` the last wide glyph covers.
        let mut covered: Option<(u16, u16, u16)> = None;
        for (x, y, cell) in updates {
            if covered.is_some_and(|(row, from, to)| y == row && (from..to).contains(&x)) {
                continue;
            }
            covered = None;
            let blanked = blanked_columns(x, cell, columns);
            if blanked > 0 {
                let blank = blanks.next().expect("one blank per wide glyph");
                for column in x + 1..=x + blanked {
                    drawn.push((column, y, blank));
                }
                covered = Some((y, x + 1, x + 1 + blanked));
            }
            drawn.push((x, y, cell));
        }
        self.inner.draw(drawn.into_iter())
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        let size = self.inner.size()?;
        self.columns.set(Some(size.width));
        Ok(size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

/// How many columns after `x` the glyph in `cell` covers that are blanked
/// before it is drawn: its width less one, cut at the terminal's right edge
/// when the width is known.
fn blanked_columns(x: u16, cell: &Cell, columns: Option<u16>) -> u16 {
    let width = match cell.diff_option {
        CellDiffOption::ForcedWidth(width) => width.get(),
        _ => cell.symbol().cell_width(),
    };
    let after = width.saturating_sub(1);
    match columns {
        Some(columns) => after.min(columns.saturating_sub(x.saturating_add(1))),
        None => after.min(u16::MAX - x),
    }
}

/// The deck's terminal: what [`ratatui::init`] sets up (a panic hook that
/// restores the terminal, raw mode, the alternate screen), drawing through a
/// [`WideGlyphBackend`] over crossterm on stdout. [`ratatui::restore`] undoes
/// it, as it undoes `ratatui::init`.
///
/// # Panics
///
/// When the terminal cannot be set up, as `ratatui::init` does.
pub fn init() -> Terminal<WideGlyphBackend<CrosstermBackend<Stdout>>> {
    try_init().expect("failed to initialize terminal")
}

fn try_init() -> io::Result<Terminal<WideGlyphBackend<CrosstermBackend<Stdout>>>> {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        hook(info);
    }));
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
    Terminal::new(WideGlyphBackend::new(CrosstermBackend::new(io::stdout())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};

    fn bytes_through(prev: &Buffer, next: &Buffer, wrapped: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        if wrapped {
            let mut backend = WideGlyphBackend::new(CrosstermBackend::new(&mut bytes));
            backend.draw(prev.diff(next).into_iter()).unwrap();
            Backend::flush(&mut backend).unwrap();
        } else {
            let mut backend = CrosstermBackend::new(&mut bytes);
            backend.draw(prev.diff(next).into_iter()).unwrap();
            Backend::flush(&mut backend).unwrap();
        }
        bytes
    }

    /// The first row of a vt100 host after `frames`, drawn one after another
    /// through the deck's backend. `one_column_vs16` drops VS16 from the
    /// output, so the host draws an emoji-presentation symbol one column wide.
    fn host_row(frames: &[Buffer], one_column_vs16: bool) -> String {
        let area = frames[0].area;
        let mut host = crate::vt_grid::parser(area.height, area.width, 0);
        let mut prev = Buffer::empty(area);
        for frame in frames {
            let bytes = String::from_utf8(bytes_through(&prev, frame, true)).unwrap();
            let bytes = if one_column_vs16 {
                bytes.replace('\u{fe0f}', "")
            } else {
                bytes
            };
            host.process(bytes.as_bytes());
            prev = frame.clone();
        }
        host.screen().contents_between(0, 0, 0, area.width)
    }

    fn row(text: &str) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        buf.set_string(0, 0, text, Style::default());
        buf
    }

    #[test]
    fn a_vs16_glyph_drawn_without_a_forced_width_redraws_as_a_fresh_render_on_either_host() {
        // `set_string` leaves the glyph's width to ratatui, whose diff follows
        // it with an update for its second column.
        let frames = [row("xZAB"), row("☀\u{fe0f}AB")];
        for one_column_vs16 in [false, true] {
            assert_eq!(
                host_row(&frames, one_column_vs16),
                host_row(&frames[1..], one_column_vs16),
                "one_column_vs16 = {one_column_vs16}"
            );
        }
        assert_eq!(host_row(&frames, true), "☀ AB");
        assert_eq!(host_row(&frames, false), "☀\u{fe0f}AB");
    }

    #[test]
    fn a_frame_with_no_wide_glyph_is_sent_as_the_crossterm_backend_sends_it() {
        let prev = row("abcdefgh");
        let mut next = row("aXcdYZgh");
        next[(3, 0)].set_style(Style::default().bg(Color::Blue));
        assert_eq!(
            bytes_through(&prev, &next, true),
            bytes_through(&prev, &next, false)
        );
    }

    #[test]
    fn a_wide_glyph_blanks_its_second_column_and_nothing_past_the_right_edge() {
        let glyph = Cell::new("你");
        assert_eq!(blanked_columns(2, &glyph, Some(8)), 1);
        assert_eq!(blanked_columns(2, &glyph, None), 1);
        assert_eq!(blanked_columns(7, &glyph, Some(8)), 0);
        assert_eq!(blanked_columns(2, &Cell::new("a"), Some(8)), 0);
        let mut forced = Cell::new("☀\u{fe0e}");
        forced.set_diff_option(CellDiffOption::ForcedWidth(
            std::num::NonZeroU16::MIN.saturating_add(1),
        ));
        assert_eq!(blanked_columns(2, &forced, Some(8)), 1);
    }
}
