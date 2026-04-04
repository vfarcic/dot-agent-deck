use std::sync::{Arc, Mutex};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

/// Converts a vt100 color to a ratatui Color.
fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Converts vt100 cell attributes to a ratatui Style.
fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    style = style.fg(vt100_color_to_ratatui(cell.fgcolor()));
    style = style.bg(vt100_color_to_ratatui(cell.bgcolor()));

    let mut modifiers = Modifier::empty();
    if cell.bold() {
        modifiers |= Modifier::BOLD;
    }
    if cell.italic() {
        modifiers |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifiers |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifiers |= Modifier::REVERSED;
    }

    style = style.add_modifier(modifiers);
    style
}

/// A Ratatui widget that renders a vt100 terminal screen.
///
/// Takes an `Arc<Mutex<vt100::Parser>>` and renders its current screen
/// contents as styled text within the given area.
pub struct TerminalWidget {
    parser: Arc<Mutex<vt100::Parser>>,
    title: String,
    focused: bool,
}

impl TerminalWidget {
    pub fn new(parser: Arc<Mutex<vt100::Parser>>, title: String, focused: bool) -> Self {
        Self {
            parser,
            title,
            focused,
        }
    }
}

impl Widget for TerminalWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let true_black = Color::Rgb(0, 0, 0);
        let border_style = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Gray)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(self.title)
            .style(Style::default().bg(true_black));

        let inner = block.inner(area);
        block.render(area, buf);

        let Ok(parser) = self.parser.lock() else {
            return;
        };
        let screen = parser.screen();

        // Render each row of the terminal screen into the inner area.
        let rows = inner.height as usize;
        let cols = inner.width as usize;

        // Calculate the visible portion: show the bottom of the screen
        // (most recent output) if the terminal has more rows than we can display.
        let screen_rows = screen.size().0 as usize;
        let start_row = screen_rows.saturating_sub(rows);

        for (y, row_idx) in (start_row..screen_rows).enumerate() {
            if y >= rows {
                break;
            }

            let mut spans = Vec::new();
            let mut col = 0;

            while col < cols {
                let cell = screen.cell(row_idx as u16, col as u16);
                match cell {
                    Some(cell) => {
                        let ch = cell.contents();
                        let style = cell_style(cell);
                        let display = if ch.is_empty() { " " } else { ch };
                        spans.push(Span::styled(display.to_string(), style));
                        col += 1;
                    }
                    None => {
                        spans.push(Span::raw(" "));
                        col += 1;
                    }
                }
            }

            let line = Line::from(spans);
            let line_area = Rect {
                x: inner.x,
                y: inner.y + y as u16,
                width: inner.width,
                height: 1,
            };
            Paragraph::new(line).render(line_area, buf);
        }

        // Render a visible block cursor when focused and not scrolled back.
        if self.focused && screen.scrollback() == 0 && !screen.hide_cursor() {
            let cursor_pos = screen.cursor_position();
            let cursor_row = cursor_pos.0 as usize;
            let cursor_col = cursor_pos.1 as usize;

            if cursor_row >= start_row && cursor_row - start_row < rows && cursor_col < cols {
                let cx = inner.x + cursor_col as u16;
                let cy = inner.y + (cursor_row - start_row) as u16;

                if let Some(existing) = buf.cell_mut((cx, cy)) {
                    existing.set_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::LightGreen)
                            .add_modifier(Modifier::BOLD),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vt100_default_color_maps_to_reset() {
        assert_eq!(vt100_color_to_ratatui(vt100::Color::Default), Color::Reset);
    }

    #[test]
    fn vt100_indexed_color_maps_correctly() {
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Idx(196)),
            Color::Indexed(196)
        );
    }

    #[test]
    fn vt100_rgb_color_maps_correctly() {
        assert_eq!(
            vt100_color_to_ratatui(vt100::Color::Rgb(255, 128, 0)),
            Color::Rgb(255, 128, 0)
        );
    }

    #[test]
    fn terminal_widget_renders_without_panic() {
        // Use a parser whose row count matches the widget inner height (10 - 2 borders = 8)
        // so text on row 0 of the parser is visible in the rendered output.
        let parser = Arc::new(Mutex::new(vt100::Parser::new(8, 38, 0)));

        // Feed some content into the parser.
        parser.lock().unwrap().process(b"Hello, terminal!");

        let widget = TerminalWidget::new(parser, "test".to_string(), true);

        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        // Row 0 is the top border. Row 1 is the first inner row (parser row 0).
        let content: String = (0..buf.area.width)
            .map(|x| {
                buf.cell((x, 1))
                    .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
            })
            .collect();
        assert!(
            content.contains("Hello, terminal!"),
            "Buffer row 1 should contain the rendered text, got: {content:?}"
        );
    }

    #[test]
    fn terminal_widget_unfocused_no_cursor() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let widget = TerminalWidget::new(parser, "test".to_string(), false);

        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
        // Should not panic — just verifying unfocused rendering works.
    }

    #[test]
    fn terminal_widget_renders_ansi_colors() {
        // Red foreground text: ESC[31m
        let parser = Arc::new(Mutex::new(vt100::Parser::new(8, 38, 0)));
        parser.lock().unwrap().process(b"\x1b[31mRed text\x1b[0m");

        let widget = TerminalWidget::new(parser, "colors".to_string(), false);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        // Verify the 'R' in "Red text" has a red-ish foreground (indexed color 1)
        let cell = buf.cell((1, 1)).unwrap(); // inner area starts at x=1,y=1
        assert_eq!(cell.symbol(), "R");
        assert_eq!(cell.fg, Color::Indexed(1)); // ANSI color 1 = red
    }

    #[test]
    fn terminal_widget_shows_cursor_when_focused() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(8, 38, 0)));
        // Cursor starts at (0,0), which maps to inner area (1,1)
        let widget = TerminalWidget::new(parser, "cursor".to_string(), true);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);

        let cell = buf.cell((1, 1)).unwrap();
        // Cursor cell should have bright green block cursor
        assert_eq!(cell.bg, Color::LightGreen);
        assert_eq!(cell.fg, Color::Black);
    }

    #[test]
    fn terminal_widget_empty_parser_no_panic() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let widget = TerminalWidget::new(parser, "empty".to_string(), true);
        let area = Rect::new(0, 0, 82, 26);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
    }

    #[test]
    fn terminal_widget_small_area_no_panic() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        parser.lock().unwrap().process(b"Hello world\nSecond line");

        let widget = TerminalWidget::new(parser, "small".to_string(), false);
        // Very small area — just 3 rows (borders + 1 inner row)
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        widget.render(area, &mut buf);
    }

    #[test]
    fn cell_style_bold_italic_underline() {
        let parser = Arc::new(Mutex::new(vt100::Parser::new(4, 40, 0)));
        // ESC[1m = bold, ESC[3m = italic, ESC[4m = underline
        parser.lock().unwrap().process(b"\x1b[1;3;4mStyled\x1b[0m");

        let screen = parser.lock().unwrap();
        let cell = screen.screen().cell(0, 0).unwrap();
        assert!(cell.bold());
        assert!(cell.italic());
        assert!(cell.underline());

        let style = cell_style(cell);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }
}
