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
//! The deck's pane widget (`crate::terminal_widget`) marks each two-column
//! cell it draws with a forced width of two, so ratatui's diff sends no update
//! for the cell's second column. For each update of such a marked cell,
//! [`WideGlyphBackend`] first blanks the second column, in the cell's own
//! style, then draws the glyph in its first column. On a host that draws the
//! glyph two columns wide the glyph covers the blank; on one that draws it one
//! column wide the second column is left blank, as a fresh render on that host
//! leaves it.
//!
//! Every update it is given reaches the wrapped backend. After a marked glyph
//! the cursor is one column or two further on, depending on the host, so an
//! update for the very next column is handed to the wrapped backend in a call
//! of its own: the crossterm backend positions the cursor explicitly for the
//! first update of each call, and so draws it in its own column on either
//! host. Any other update that follows a glyph is not in the column after
//! the last one the crossterm backend drew, so it positions the cursor for
//! that update itself.
//!
//! Cells the pane widget did not mark, such as a wide glyph another widget
//! writes with `set_string`, are sent exactly as the crossterm backend sends
//! them; how ratatui draws those is unchanged by this backend.

use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellDiffOption};
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
        // Each update's blank and how many columns it covers, made before
        // any update borrows one, and kept beside the update it belongs to.
        let blanks: Vec<Option<(Cell, u16)>> = updates
            .iter()
            .map(|&(x, _, cell)| {
                let blanked = blanked_columns(x, cell, columns);
                (blanked > 0).then(|| {
                    let mut blank = cell.clone();
                    blank.set_symbol(" ").set_diff_option(CellDiffOption::None);
                    (blank, blanked)
                })
            })
            .collect();
        let mut call: Vec<(u16, u16, &Cell)> = Vec::with_capacity(updates.len());
        for (index, (&(x, y, cell), blank)) in updates.iter().zip(&blanks).enumerate() {
            if let Some((blank, blanked)) = blank {
                // `blanked_columns` stops at the terminal's last column, so
                // `x + blanked` neither passes it nor overflows.
                for column in x + 1..=x + blanked {
                    call.push((column, y, blank));
                }
            }
            call.push((x, y, cell));
            let next_is_the_column_after =
                updates.get(index + 1).is_some_and(|&(next_x, next_y, _)| {
                    next_y == y && Some(next_x) == x.checked_add(1)
                });
            if marked_wide(cell) && next_is_the_column_after {
                self.inner.draw(std::mem::take(&mut call).into_iter())?;
            }
        }
        self.inner.draw(call.into_iter())
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

/// How many columns after `x` are blanked before the glyph in `cell` is
/// drawn: for a cell the pane widget marked with a forced width, that width
/// less one, cut at the terminal's right edge, or before the terminal's width
/// is known at the last column a `u16` can name. None for any other cell. So
/// `x + blanked_columns(..)` is always a column a `u16` holds.
fn blanked_columns(x: u16, cell: &Cell, columns: Option<u16>) -> u16 {
    let CellDiffOption::ForcedWidth(width) = cell.diff_option else {
        return 0;
    };
    let after = width.get() - 1;
    match columns {
        Some(columns) => after.min(columns.saturating_sub(x.saturating_add(1))),
        None => after.min(u16::MAX - x),
    }
}

/// Whether the pane widget marked `cell` as taking more than one column.
fn marked_wide(cell: &Cell) -> bool {
    matches!(cell.diff_option, CellDiffOption::ForcedWidth(width) if width.get() > 1)
}

/// The deck's terminal: raw mode and the alternate screen, drawing through a
/// [`WideGlyphBackend`] over crossterm on stdout. [`ratatui::restore`] undoes
/// it.
///
/// Unlike [`ratatui::init`], it installs no panic hook. A hook that hands the
/// terminal back unconditionally would run before any hook installed earlier,
/// so a panic the deck contains and recovers from (`crate::ui::tui_panic_hook`)
/// would still leave the running TUI drawing on a restored terminal. The
/// caller's own hook is what hands the terminal back; the deck installs it
/// before calling this (`crate::ui::enter_tui_terminal`).
///
/// # Panics
///
/// When the terminal cannot be set up, as `ratatui::init` does, after handing
/// back whatever of it was set up.
pub fn init() -> DeckTerminal {
    try_init().expect("failed to initialize terminal")
}

/// The terminal [`init`] sets up.
pub type DeckTerminal = Terminal<WideGlyphBackend<CrosstermBackend<Stdout>>>;

fn try_init() -> io::Result<DeckTerminal> {
    crossterm::terminal::enable_raw_mode()?;
    let terminal = crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)
        .and_then(|()| Terminal::new(WideGlyphBackend::new(CrosstermBackend::new(io::stdout()))));
    if terminal.is_err() {
        ratatui::restore();
    }
    terminal
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    use std::num::NonZeroU16;

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

    fn row(text: &str) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        buf.set_string(0, 0, text, Style::default());
        buf
    }

    /// A glyph as the pane widget draws a two-column cell: its width forced
    /// to the screen's two columns.
    fn pane_glyph(symbol: &'static str, style: Style) -> Cell {
        let mut cell = Cell::new(symbol);
        cell.set_style(style)
            .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::new(2).unwrap()));
        cell
    }

    fn narrow(symbol: &'static str) -> Cell {
        Cell::new(symbol)
    }

    /// One update as the wrapped backend received it.
    #[derive(Debug, Clone, PartialEq)]
    struct Drawn {
        x: u16,
        y: u16,
        symbol: String,
        bg: Color,
    }

    /// A backend that records each `draw` call's updates, one `Vec` per call.
    struct Recorder {
        calls: Vec<Vec<Drawn>>,
        width: u16,
    }

    impl Recorder {
        fn new(width: u16) -> Self {
            Self {
                calls: Vec::new(),
                width,
            }
        }
    }

    impl Backend for Recorder {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.calls.push(
                content
                    .map(|(x, y, cell)| Drawn {
                        x,
                        y,
                        symbol: cell.symbol().to_owned(),
                        bg: cell.bg,
                    })
                    .collect(),
            );
            Ok(())
        }
        fn hide_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(Position::ORIGIN)
        }
        fn set_cursor_position<P: Into<Position>>(&mut self, _position: P) -> io::Result<()> {
            Ok(())
        }
        fn clear(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn clear_region(&mut self, _clear_type: ClearType) -> io::Result<()> {
            Ok(())
        }
        fn size(&self) -> io::Result<Size> {
            Ok(Size::new(self.width, 1))
        }
        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: Size::new(self.width, 1),
                pixels: Size::new(0, 0),
            })
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The draw calls the wrapped backend receives for `updates`, on a
    /// terminal `width` columns wide whose size was queried first (as
    /// `Terminal::new` does), or not when `width` is `None`.
    fn recorded(width: Option<u16>, updates: &[(u16, u16, Cell)]) -> Vec<Vec<Drawn>> {
        let mut backend = WideGlyphBackend::new(Recorder::new(width.unwrap_or(8)));
        if width.is_some() {
            backend.size().unwrap();
        }
        backend
            .draw(updates.iter().map(|(x, y, cell)| (*x, *y, cell)))
            .unwrap();
        backend.inner.calls
    }

    fn drawn(x: u16, symbol: &str, bg: Color) -> Drawn {
        Drawn {
            x,
            y: 0,
            symbol: symbol.to_owned(),
            bg,
        }
    }

    /// The first row of a vt100 host after `updates` are drawn through the
    /// deck's backend over crossterm. `one_column_vs16` drops VS16 from the
    /// output, so the host draws an emoji-presentation symbol one column wide.
    fn host_after(updates: &[(u16, u16, Cell)], one_column_vs16: bool) -> vt100::Parser {
        let mut bytes = Vec::new();
        {
            let mut backend = WideGlyphBackend::new(CrosstermBackend::new(&mut bytes));
            backend.columns.set(Some(8));
            backend
                .draw(updates.iter().map(|(x, y, cell)| (*x, *y, cell)))
                .unwrap();
            Backend::flush(&mut backend).unwrap();
        }
        let bytes = String::from_utf8(bytes).unwrap();
        let bytes = if one_column_vs16 {
            bytes.replace('\u{fe0f}', "")
        } else {
            bytes
        };
        let mut host = crate::vt_grid::parser(1, 8, 0);
        host.process(bytes.as_bytes());
        host
    }

    #[test]
    fn a_pane_glyph_gets_its_second_column_blanked_in_its_own_style_first() {
        let red = Style::default().bg(Color::Red);
        assert_eq!(
            recorded(Some(8), &[(2, 0, pane_glyph("☀\u{fe0f}", red))]),
            vec![vec![
                drawn(3, " ", Color::Red),
                drawn(2, "☀\u{fe0f}", Color::Red)
            ]]
        );
    }

    #[test]
    fn an_update_for_the_column_after_a_pane_glyph_is_kept_and_lands_in_that_column() {
        let updates = [
            (0, 0, pane_glyph("☀\u{fe0f}", Style::default())),
            (1, 0, narrow("A")),
        ];
        let calls = recorded(Some(8), &updates);
        assert!(
            calls
                .iter()
                .flatten()
                .any(|d| d == &drawn(1, "A", Color::Reset)),
            "the update for column 1 reached the wrapped backend: {calls:?}"
        );
        for one_column_vs16 in [false, true] {
            let host = host_after(&updates, one_column_vs16);
            assert_eq!(
                host.screen().cell(0, 1).map(|c| c.contents().to_owned()),
                Some("A".to_owned()),
                "one_column_vs16 = {one_column_vs16}: {:?}",
                host.screen().contents()
            );
        }
    }

    #[test]
    fn each_blank_takes_the_style_of_the_glyph_it_belongs_to() {
        let style = |bg| Style::default().bg(bg);
        let updates = [
            (0, 0, pane_glyph("☀\u{fe0f}", style(Color::Red))),
            (1, 0, pane_glyph("你", style(Color::Blue))),
            (4, 0, pane_glyph("好", style(Color::Green))),
        ];
        let blanks: Vec<Drawn> = recorded(Some(8), &updates)
            .into_iter()
            .flatten()
            .filter(|d| d.symbol == " ")
            .collect();
        assert_eq!(
            blanks,
            vec![
                drawn(1, " ", Color::Red),
                drawn(2, " ", Color::Blue),
                drawn(5, " ", Color::Green),
            ]
        );
    }

    #[test]
    fn updates_in_any_order_all_reach_the_wrapped_backend() {
        let updates = [
            (5, 0, narrow("B")),
            (2, 0, pane_glyph("你", Style::default())),
            (3, 0, narrow("C")),
            (0, 0, narrow("A")),
        ];
        let calls: Vec<Drawn> = recorded(Some(8), &updates).into_iter().flatten().collect();
        for expected in [
            drawn(5, "B", Color::Reset),
            drawn(2, "你", Color::Reset),
            drawn(3, "C", Color::Reset),
            drawn(0, "A", Color::Reset),
        ] {
            assert!(calls.contains(&expected), "{expected:?} missing: {calls:?}");
        }
    }

    #[test]
    fn a_wide_glyph_the_pane_did_not_mark_is_sent_as_the_crossterm_backend_sends_it() {
        // `set_string` leaves the glyph's width to ratatui, whose diff follows
        // a VS16 glyph with an update for its second column. How a host draws
        // that is ratatui's own behaviour, which this backend leaves alone.
        for (prev, next) in [("xZAB", "☀\u{fe0f}AB"), ("xZAB", "你AB")] {
            assert_eq!(
                bytes_through(&row(prev), &row(next), true),
                bytes_through(&row(prev), &row(next), false),
                "{prev} -> {next}"
            );
        }
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
    fn a_pane_glyph_in_the_last_column_gets_no_blank_whether_the_size_was_queried_or_not() {
        let glyph = |x| [(x, 0, pane_glyph("你", Style::default()))];
        let only_the_glyph = |x| vec![vec![drawn(x, "你", Color::Reset)]];
        let blank_then_glyph = |x| {
            vec![vec![
                drawn(x + 1, " ", Color::Reset),
                drawn(x, "你", Color::Reset),
            ]]
        };
        // Queried: the terminal's last column gets no blank past it.
        assert_eq!(recorded(Some(8), &glyph(7)), only_the_glyph(7));
        assert_eq!(recorded(Some(8), &glyph(6)), blank_then_glyph(6));
        assert_eq!(
            recorded(Some(u16::MAX), &glyph(u16::MAX - 1)),
            only_the_glyph(u16::MAX - 1)
        );
        // Not queried: the last column a `u16` names is the edge, and
        // reaching it neither panics nor wraps.
        assert_eq!(recorded(None, &glyph(2)), blank_then_glyph(2));
        assert_eq!(
            recorded(None, &glyph(u16::MAX - 1)),
            blank_then_glyph(u16::MAX - 1)
        );
        assert_eq!(recorded(None, &glyph(u16::MAX)), only_the_glyph(u16::MAX));
    }
}
