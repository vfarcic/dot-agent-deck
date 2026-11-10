//! Keyboard copy mode (PRD #385 M5): select text in the focused pane, its
//! history included, with the keyboard, and copy it.
//!
//! It exists for setups where the mouse cannot select: a terminal that does
//! not pass mouse reports through, or an ssh session in one. `v` in command
//! mode opens it on the focused pane; the keys are limited to select-and-yank
//! (`ui::handle_copy_mode_key` maps them to the operations here).
//!
//! **The view is frozen.** Copy mode works on a copy of the pane's terminal
//! screen taken when it opens (`vt100::Screen` is `Clone`), not on the live
//! one. Output that arrives while copy mode is open goes to the live screen
//! only, so the text under the cursor never moves and the highlight always
//! shows exactly what a yank copies (PRD #385 decision 6). Paging moves the
//! copy's scrollback offset, never the live pane's, so leaving copy mode
//! leaves the pane at the scroll position it had (vt100 itself keeps a pane
//! that was scrolled back anchored to its text as new lines arrive).
//!
//! Selection is a stream, as a mouse drag is: from the anchor to the cursor,
//! running to the end of each line in between.

use std::sync::{Arc, Mutex};

/// A cell in copy mode's text. `line` counts from the oldest line of the
/// pane's history (0) through the history and then the screen's rows; `col`
/// is a screen column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: u16,
}

/// A cursor movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Left,
    Right,
    Up,
    Down,
    /// Back one screen into the history.
    PageUp,
    /// Forward one screen.
    PageDown,
    /// The first line of the history.
    Top,
    /// The last line of the screen.
    Bottom,
}

/// One open copy mode, on one pane.
pub struct CopyMode {
    pane_id: String,
    /// The pane's screen as it was when copy mode opened. Its scrollback
    /// offset is copy mode's view.
    frozen: Arc<Mutex<vt100::Parser>>,
    history: usize,
    rows: u16,
    cols: u16,
    /// Scrollback offset of the view (0 = the screen's own rows).
    view: usize,
    cursor: Pos,
    anchor: Option<Pos>,
}

/// What one visible row of copy mode's view shows besides its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RowMarks {
    /// The selected columns, first to last, inclusive.
    pub selected: Option<(u16, u16)>,
    /// The cursor's column, when the cursor is on this row.
    pub cursor: Option<u16>,
}

impl CopyMode {
    /// Open copy mode on `pane_id`, whose live screen is `screen`. The view
    /// starts where the pane is scrolled to, and the cursor at the pane's own
    /// cursor when the pane shows its live screen, or on the view's last row
    /// when it is scrolled back.
    pub fn open(pane_id: impl Into<String>, screen: &vt100::Screen) -> Self {
        let (rows, cols) = crate::vt_grid::size(screen);
        let mut parser = crate::vt_grid::parser(rows, cols, 0);
        *parser.screen_mut() = screen.clone();
        let history = screen.scrollback_len();
        let view = screen.scrollback().min(history);
        let cursor = if view == 0 {
            let (row, col) = screen.cursor_position();
            Pos {
                line: history + usize::from(row.min(rows - 1)),
                col: col.min(cols - 1),
            }
        } else {
            Pos {
                line: history - view + usize::from(rows - 1),
                col: 0,
            }
        };
        Self {
            pane_id: pane_id.into(),
            frozen: Arc::new(Mutex::new(parser)),
            history,
            rows,
            cols,
            view,
            cursor,
            anchor: None,
        }
    }

    pub fn pane_id(&self) -> &str {
        &self.pane_id
    }

    /// The frozen screen, scrolled to copy mode's view, for drawing.
    pub fn frozen(&self) -> Arc<Mutex<vt100::Parser>> {
        Arc::clone(&self.frozen)
    }

    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    fn total_lines(&self) -> usize {
        self.history + usize::from(self.rows)
    }

    /// The line shown on the view's first row.
    fn top(&self) -> usize {
        self.history - self.view
    }

    fn page(&self) -> usize {
        usize::from(self.rows.saturating_sub(1).max(1))
    }

    pub fn apply(&mut self, movement: Move) {
        let last_line = self.total_lines() - 1;
        let last_col = self.cols - 1;
        let page = self.page();
        match movement {
            Move::Left => self.cursor.col = self.cursor.col.saturating_sub(1),
            Move::Right => self.cursor.col = (self.cursor.col + 1).min(last_col),
            Move::Up => self.cursor.line = self.cursor.line.saturating_sub(1),
            Move::Down => self.cursor.line = (self.cursor.line + 1).min(last_line),
            Move::PageUp => {
                self.view = (self.view + page).min(self.history);
                self.cursor.line = self.cursor.line.saturating_sub(page);
            }
            Move::PageDown => {
                self.view = self.view.saturating_sub(page);
                self.cursor.line = (self.cursor.line + page).min(last_line);
            }
            Move::Top => {
                self.view = self.history;
                self.cursor = Pos { line: 0, col: 0 };
            }
            Move::Bottom => {
                self.view = 0;
                self.cursor = Pos {
                    line: last_line,
                    col: 0,
                };
            }
        }
        self.keep_cursor_in_view();
        self.scroll_frozen_to(self.view);
    }

    /// Scroll the view just enough to show the cursor.
    fn keep_cursor_in_view(&mut self) {
        let rows = usize::from(self.rows);
        let top = self.top();
        if self.cursor.line < top {
            self.view = self.history - self.cursor.line;
        } else if self.cursor.line >= top + rows {
            self.view = self.history - (self.cursor.line + 1 - rows);
        }
    }

    fn scroll_frozen_to(&self, offset: usize) {
        if let Ok(mut parser) = self.frozen.lock() {
            parser.screen_mut().set_scrollback(offset);
        }
    }

    /// Start the selection at the cursor, or drop the one started.
    pub fn toggle_anchor(&mut self) {
        self.anchor = match self.anchor {
            Some(_) => None,
            None => Some(self.cursor),
        };
    }

    /// The selection, first cell to last, inclusive.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        Some(if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    /// What the view's `row` shows besides its text.
    pub fn row_marks(&self, row: u16) -> RowMarks {
        let line = self.top() + usize::from(row);
        let selected = self.selection().and_then(|(start, end)| {
            (start.line..=end.line).contains(&line).then(|| {
                let first = if line == start.line { start.col } else { 0 };
                let last = if line == end.line {
                    end.col
                } else {
                    self.cols - 1
                };
                (first, last)
            })
        });
        RowMarks {
            selected,
            cursor: (self.cursor.line == line).then_some(self.cursor.col),
        }
    }

    /// The selected text, or `None` with nothing selected. A line that ends
    /// inside the selection loses its trailing blanks and ends in a newline,
    /// unless the terminal wrapped it onto the next one.
    pub fn yank(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        let Ok(mut parser) = self.frozen.lock() else {
            return None;
        };
        let mut text = String::new();
        for line in start.line..=end.line {
            let (offset, row) = if line < self.history {
                (self.history - line, 0)
            } else {
                (0, (line - self.history) as u16)
            };
            parser.screen_mut().set_scrollback(offset);
            let screen = parser.screen();
            let first = if line == start.line { start.col } else { 0 };
            let last = if line == end.line {
                end.col
            } else {
                self.cols - 1
            };
            let mut segment = String::new();
            for col in first..=last.min(self.cols - 1) {
                match screen.cell(row, col) {
                    Some(cell) if cell.is_wide_continuation() => {}
                    Some(cell) if !cell.contents().is_empty() => segment.push_str(cell.contents()),
                    _ => segment.push(' '),
                }
            }
            if line == end.line {
                text.push_str(segment.trim_end());
            } else if screen.row_wrapped(row) && last == self.cols - 1 {
                text.push_str(&segment);
            } else {
                text.push_str(segment.trim_end());
                text.push('\n');
            }
        }
        parser.screen_mut().set_scrollback(self.view);
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = crate::vt_grid::parser(rows, cols, 100);
        parser.process(bytes);
        parser
    }

    #[test]
    fn a_stream_selection_runs_to_the_end_of_each_line_between() {
        let parser = screen(4, 10, b"alpha one\r\nbravo two\r\ncharlie\r\n");
        let mut copy = CopyMode::open("p", parser.screen());
        copy.apply(Move::Top);
        copy.apply(Move::Right);
        copy.apply(Move::Right);
        copy.toggle_anchor();
        copy.apply(Move::Down);
        copy.apply(Move::Down);
        copy.apply(Move::Right);
        assert_eq!(copy.yank().as_deref(), Some("pha one\nbravo two\nchar"));
    }

    #[test]
    fn a_wrapped_line_is_copied_without_a_newline() {
        let parser = screen(4, 5, b"abcdefgh");
        let mut copy = CopyMode::open("p", parser.screen());
        copy.apply(Move::Top);
        copy.toggle_anchor();
        copy.apply(Move::Down);
        copy.apply(Move::Right);
        copy.apply(Move::Right);
        assert_eq!(copy.yank().as_deref(), Some("abcdefgh"));
    }

    #[test]
    fn the_cursor_keeps_the_view_on_itself_through_the_history() {
        let lines: Vec<String> = (1..=20).map(|n| format!("l{n}")).collect();
        let parser = screen(4, 10, lines.join("\r\n").as_bytes());
        let mut copy = CopyMode::open("p", parser.screen());
        assert_eq!(copy.cursor().line, 19);
        for _ in 0..19 {
            copy.apply(Move::Up);
        }
        assert_eq!(copy.cursor(), Pos { line: 0, col: 3 });
        assert_eq!(copy.row_marks(0).cursor, Some(3));
        copy.apply(Move::PageDown);
        assert_eq!(copy.cursor().line, 3);
        copy.apply(Move::Bottom);
        assert_eq!(copy.cursor(), Pos { line: 19, col: 0 });
        assert_eq!(copy.row_marks(3).cursor, Some(0));
    }
}
