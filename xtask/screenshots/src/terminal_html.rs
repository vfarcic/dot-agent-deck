//! A `vt100::Screen` rendered as a standalone HTML page.
//!
//! The TUI half of the docs screenshots (issue #1322) is a frame of the real
//! binary as the L2 harness's vt100 parser holds it — every cell with its
//! character, colours and attributes. This module turns that frame into HTML;
//! Playwright's Chromium then rasterizes the HTML to PNG, the same engine and
//! settings the desktop screenshots come out of.
//!
//! Why HTML and not a Rust rasterizer: a font rasterizer in Rust is a heavy
//! dependency for one job, and it would be a second rendering stack whose
//! output drifts from the desktop images for reasons neither side controls.
//! One Chromium is one set of rendering rules.
//!
//! The page is deterministic by construction: no timestamps, no randomness,
//! and cells are emitted in row-major order with a fixed palette, so the same
//! screen always produces the same bytes.

use std::fmt::Write as _;

/// The colours a rendered frame uses for the terminal's default foreground and
/// background, and for the sixteen ANSI colours. Indices 16–255 are the
/// standard xterm cube and grey ramp and are computed, not configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub foreground: (u8, u8, u8),
    pub background: (u8, u8, u8),
    pub ansi: [(u8, u8, u8); 16],
}

impl Default for Palette {
    /// A dark theme close to the terminals the existing hand-taken docs images
    /// were captured in, so regenerated images sit beside them without a jump.
    fn default() -> Self {
        Self {
            foreground: (0xd4, 0xd7, 0xdc),
            background: (0x17, 0x1a, 0x1f),
            ansi: [
                (0x1e, 0x21, 0x27),
                (0xe0, 0x6c, 0x75),
                (0x98, 0xc3, 0x79),
                (0xe5, 0xc0, 0x7b),
                (0x61, 0xaf, 0xef),
                (0xc6, 0x78, 0xdd),
                (0x56, 0xb6, 0xc2),
                (0xd4, 0xd7, 0xdc),
                (0x5c, 0x63, 0x70),
                (0xf0, 0x7f, 0x88),
                (0xb5, 0xe0, 0x96),
                (0xf2, 0xd4, 0x9b),
                (0x8c, 0xc6, 0xf7),
                (0xdc, 0x9e, 0xf0),
                (0x7f, 0xd4, 0xde),
                (0xff, 0xff, 0xff),
            ],
        }
    }
}

impl Palette {
    /// The RGB value of one of the 256 indexed colours.
    pub fn indexed(&self, idx: u8) -> (u8, u8, u8) {
        match idx {
            0..=15 => self.ansi[idx as usize],
            16..=231 => {
                let n = idx - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                (level(n / 36), level((n / 6) % 6), level(n % 6))
            }
            232..=255 => {
                let grey = 8 + (idx - 232) * 10;
                (grey, grey, grey)
            }
        }
    }

    fn resolve(&self, color: vt100::Color, default: (u8, u8, u8)) -> (u8, u8, u8) {
        match color {
            vt100::Color::Default => default,
            vt100::Color::Idx(idx) => self.indexed(idx),
            vt100::Color::Rgb(r, g, b) => (r, g, b),
        }
    }
}

/// How a frame is laid out on the page.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOptions {
    pub palette: Palette,
    /// CSS font stack. The first family that is installed wins, so the
    /// maintainer doc names the one the committed images were made with.
    pub font_family: String,
    /// Font size in CSS pixels.
    pub font_size_px: u16,
    /// Line height as a multiple of the font size. Box-drawing glyphs only
    /// join up vertically when this is no taller than the glyphs themselves.
    pub line_height: f32,
    /// Padding around the grid, in CSS pixels, painted in the default
    /// background colour.
    pub padding_px: u16,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            palette: Palette::default(),
            font_family: "\"DejaVu Sans Mono\", \"Liberation Mono\", monospace".to_string(),
            font_size_px: 14,
            line_height: 1.2,
            padding_px: 12,
        }
    }
}

/// The resolved look of one cell. Consecutive cells with the same style share
/// one `<span>`, which keeps the page small without changing a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    bold: bool,
    italic: bool,
    underline: bool,
}

fn cell_style(cell: &vt100::Cell, palette: &Palette) -> CellStyle {
    let mut fg = palette.resolve(cell.fgcolor(), palette.foreground);
    let mut bg = palette.resolve(cell.bgcolor(), palette.background);
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.dim() {
        // Half way to the background, which is how most terminals draw SGR 2.
        fg = (
            ((u16::from(fg.0) + u16::from(bg.0)) / 2) as u8,
            ((u16::from(fg.1) + u16::from(bg.1)) / 2) as u8,
            ((u16::from(fg.2) + u16::from(bg.2)) / 2) as u8,
        );
    }
    CellStyle {
        fg,
        bg,
        bold: cell.bold(),
        italic: cell.italic(),
        underline: cell.underline(),
    }
}

fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn push_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

fn open_span(
    out: &mut String,
    style: &CellStyle,
    default_fg: (u8, u8, u8),
    default_bg: (u8, u8, u8),
) {
    let mut css = String::new();
    if style.fg != default_fg {
        let _ = write!(css, "color:{};", hex(style.fg));
    }
    if style.bg != default_bg {
        let _ = write!(css, "background:{};", hex(style.bg));
    }
    if style.bold {
        css.push_str("font-weight:700;");
    }
    if style.italic {
        css.push_str("font-style:italic;");
    }
    if style.underline {
        css.push_str("text-decoration:underline;");
    }
    if css.is_empty() {
        out.push_str("<span>");
    } else {
        let _ = write!(out, "<span style=\"{css}\">");
    }
}

/// The grid alone: one `<div class="row">` per terminal row, each row's cells
/// grouped into styled spans. Every cell is exactly one grid column wide; a
/// wide character is emitted once, with a fixed two-column width, and its
/// continuation cell is skipped. The cursor is not drawn — the TUI hides it on
/// every screen a docs image shows, and a stray block would be noise.
pub fn render_rows(screen: &vt100::Screen, palette: &Palette) -> String {
    let (rows, cols) = screen.size();
    let mut out = String::new();
    for row in 0..rows {
        out.push_str("<div class=\"row\">");
        let mut current: Option<CellStyle> = None;
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let style = cell_style(cell, palette);
            if current != Some(style) {
                if current.is_some() {
                    out.push_str("</span>");
                }
                open_span(&mut out, &style, palette.foreground, palette.background);
                current = Some(style);
            }
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            if cell.is_wide() {
                out.push_str("<span class=\"wide\">");
                push_escaped(&mut out, text);
                out.push_str("</span>");
            } else {
                push_escaped(&mut out, text);
            }
        }
        if current.is_some() {
            out.push_str("</span>");
        }
        out.push_str("</div>\n");
    }
    out
}

/// A complete, self-contained HTML page showing `screen`. The grid sits in an
/// element with id `terminal`, sized to exactly `cols × rows` character cells
/// plus padding, so an element screenshot of it is the frame and nothing else.
pub fn render_page(screen: &vt100::Screen, title: &str, options: &RenderOptions) -> String {
    let (rows, cols) = screen.size();
    let palette = &options.palette;
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>");
    push_escaped(&mut out, title);
    out.push_str("</title>\n<style>\n");
    let _ = writeln!(
        out,
        "html, body {{ margin: 0; padding: 0; background: {bg}; }}",
        bg = hex(palette.background)
    );
    let _ = writeln!(
        out,
        "#terminal {{ display: inline-block; padding: {pad}px; background: {bg}; color: {fg}; \
         font-family: {font}; font-size: {size}px; line-height: {lh}; \
         font-variant-ligatures: none; font-kerning: none; \
         -webkit-font-smoothing: antialiased; }}",
        pad = options.padding_px,
        bg = hex(palette.background),
        fg = hex(palette.foreground),
        font = options.font_family,
        size = options.font_size_px,
        lh = options.line_height,
    );
    // `ch` is the advance of "0" in the element's font, i.e. one monospace
    // cell, so these widths hold whatever font the stack resolves to. The
    // container is NOT `white-space: pre`: the newline after each row's
    // `</div>` would then render as a blank line between rows.
    let _ = writeln!(
        out,
        ".row {{ width: {cols}ch; height: {lh}em; overflow: hidden; white-space: pre; }}",
        lh = options.line_height
    );
    out.push_str(".wide { display: inline-block; width: 2ch; }\n");
    out.push_str("</style>\n</head>\n<body>\n");
    let _ = writeln!(
        out,
        "<div id=\"terminal\" data-rows=\"{rows}\" data-cols=\"{cols}\">"
    );
    out.push_str(&render_rows(screen, palette));
    out.push_str("</div>\n</body>\n</html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        parser
    }

    #[test]
    fn renders_one_div_per_row_padded_to_the_full_width() {
        let parser = screen(3, 5, b"hi");
        let rows = render_rows(parser.screen(), &Palette::default());
        assert_eq!(rows.matches("<div class=\"row\">").count(), 3);
        // Blank cells are spaces, so every row is exactly `cols` characters.
        assert!(rows.contains("<div class=\"row\"><span>hi   </span></div>"));
        assert!(rows.contains("<div class=\"row\"><span>     </span></div>"));
    }

    #[test]
    fn indexed_rgb_and_default_colours_resolve_through_the_palette() {
        let palette = Palette::default();
        // SGR 31 (ANSI red), then 38;5;196 (cube red), then 38;2 (truecolour).
        let parser = screen(1, 3, b"\x1b[31mA\x1b[38;5;196mB\x1b[38;2;1;2;3mC");
        let rows = render_rows(parser.screen(), &palette);
        assert!(rows.contains(&format!("color:{};", hex(palette.ansi[1]))));
        assert!(rows.contains("color:#ff0000;"));
        assert!(rows.contains("color:#010203;"));
        assert_eq!(palette.indexed(16), (0, 0, 0));
        assert_eq!(palette.indexed(231), (255, 255, 255));
        assert_eq!(palette.indexed(232), (8, 8, 8));
        assert_eq!(palette.indexed(255), (238, 238, 238));
    }

    #[test]
    fn background_colours_survive() {
        let palette = Palette::default();
        let parser = screen(1, 2, b"\x1b[44mX");
        let rows = render_rows(parser.screen(), &palette);
        assert!(rows.contains(&format!("background:{};", hex(palette.ansi[4]))));
    }

    #[test]
    fn inverse_swaps_foreground_and_background() {
        let palette = Palette::default();
        let parser = screen(1, 1, b"\x1b[7mX");
        let rows = render_rows(parser.screen(), &palette);
        assert!(rows.contains(&format!(
            "color:{};background:{};",
            hex(palette.background),
            hex(palette.foreground)
        )));
    }

    #[test]
    fn bold_italic_underline_and_dim_are_all_carried() {
        let palette = Palette {
            foreground: (200, 200, 200),
            background: (0, 0, 0),
            ..Palette::default()
        };
        let parser = screen(
            1,
            4,
            b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[4mU\x1b[0m\x1b[2mD",
        );
        let rows = render_rows(parser.screen(), &palette);
        assert!(rows.contains("<span style=\"font-weight:700;\">B</span>"));
        assert!(rows.contains("<span style=\"font-style:italic;\">I</span>"));
        assert!(rows.contains("<span style=\"text-decoration:underline;\">U</span>"));
        // Dim is half way from the foreground to the background.
        assert!(rows.contains("<span style=\"color:#646464;\">D</span>"));
    }

    #[test]
    fn same_styled_cells_share_one_span() {
        let parser = screen(1, 6, b"\x1b[32mgreen\x1b[0m!");
        let rows = render_rows(parser.screen(), &Palette::default());
        assert_eq!(rows.matches("<span").count(), 2);
    }

    #[test]
    fn markup_characters_are_escaped() {
        let parser = screen(1, 8, b"<a&b>\"");
        let rows = render_rows(parser.screen(), &Palette::default());
        assert!(rows.contains("&lt;a&amp;b&gt;&quot;"));
        assert!(!rows.contains("<a"));
    }

    #[test]
    fn a_wide_character_is_emitted_once_at_two_columns() {
        let parser = screen(1, 4, "日x".as_bytes());
        let rows = render_rows(parser.screen(), &Palette::default());
        assert_eq!(rows.matches('日').count(), 1);
        assert!(rows.contains("<span class=\"wide\">日</span>x "));
    }

    #[test]
    fn box_drawing_passes_through_untouched() {
        let parser = screen(1, 3, "┌─┐".as_bytes());
        let rows = render_rows(parser.screen(), &Palette::default());
        assert!(rows.contains("┌─┐"));
    }

    #[test]
    fn the_page_is_self_contained_and_sized_to_the_grid() {
        let parser = screen(2, 7, b"x");
        let page = render_page(parser.screen(), "a <title>", &RenderOptions::default());
        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("<title>a &lt;title&gt;</title>"));
        assert!(page.contains("<div id=\"terminal\" data-rows=\"2\" data-cols=\"7\">"));
        assert!(page.contains(".row { width: 7ch;"));
        // Nothing the page loads from elsewhere: a remote font or stylesheet
        // would make the image depend on the network.
        assert!(!page.contains("http"));
        assert!(!page.contains("<link"));
        assert!(!page.contains("<script"));
    }

    #[test]
    fn rendering_is_deterministic() {
        let bytes = b"\x1b[1;36mdot-agent-deck\x1b[0m \xe2\x94\x80 3 session(s)\r\n\x1b[33m\xe2\x97\x8f\x1b[0m Working";
        let a = screen(4, 40, bytes);
        let b = screen(4, 40, bytes);
        let options = RenderOptions::default();
        assert_eq!(
            render_page(a.screen(), "t", &options),
            render_page(b.screen(), "t", &options)
        );
    }
}
