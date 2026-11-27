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
//!
//! **One page, two themes (PR #2491).** A cell drawn in the terminal's default
//! colours or one of the sixteen ANSI colours — every colour the TUI itself
//! chooses, since `src/palette.rs` allows named ANSI colours only — refers to a
//! CSS variable rather than a hex value, and the page defines those variables
//! twice: the dark palette by default, the light one under
//! `prefers-color-scheme: light` (unless `data-theme="dark"` is set on the root)
//! or `data-theme="light"`. The rasterizer screenshots the same page once per
//! theme, so the TUI gets a light and a dark image of one frame. Colours that
//! are not the terminal's to remap (the 256-colour cube, the grey ramp and
//! truecolour) are written as hex in both.

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
    /// [`Palette::dark`], the theme the committed docs images are in.
    fn default() -> Self {
        Self::dark()
    }
}

impl Palette {
    /// A dark theme close to the terminals the existing hand-taken docs images
    /// were captured in, so regenerated images sit beside them without a jump.
    pub fn dark() -> Self {
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

    /// A light terminal theme (PR #2491), in the spirit of One Light, the light
    /// sibling of [`Palette::dark`]'s One Dark. Every one of the sixteen ANSI
    /// colours, normal and bright, reads at 4.5:1 or more on the background,
    /// so a status word the TUI colours is as legible as on a light terminal a
    /// user would pick. That includes white (7) and bright white (15), which
    /// are mapped to greys darker than the background rather than kept light:
    /// the TUI draws text in both — the project-trust prompt's labels in white
    /// and the commands it asks the user to approve in bright white
    /// (`src/trust_prompt.rs`) — so bright white is the darker of the two
    /// (14.5:1, against white's 5.7:1), as it is the brighter on the dark
    /// theme, and a light image shows what that screen asks.
    pub fn light() -> Self {
        Self {
            foreground: (0x38, 0x3a, 0x42),
            background: (0xfa, 0xfa, 0xfa),
            ansi: [
                (0x38, 0x3a, 0x42),
                (0xc8, 0x38, 0x2c),
                (0x2f, 0x7d, 0x32),
                (0x8a, 0x61, 0x00),
                (0x2f, 0x62, 0xd0),
                (0xa0, 0x2b, 0xa0),
                (0x0a, 0x7a, 0x8f),
                (0x61, 0x64, 0x6d),
                (0x6b, 0x6e, 0x78),
                (0xb3, 0x26, 0x1e),
                (0x25, 0x6b, 0x29),
                (0x73, 0x4f, 0x00),
                (0x24, 0x49, 0xad),
                (0x86, 0x22, 0x8a),
                (0x08, 0x69, 0x7a),
                (0x24, 0x26, 0x2c),
            ],
        }
    }

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

    /// The CSS custom properties that define this palette, as declarations.
    fn variables(&self) -> String {
        let mut out = format!(
            "--fg: {}; --bg: {};",
            hex(self.foreground),
            hex(self.background)
        );
        for (idx, rgb) in self.ansi.iter().enumerate() {
            let _ = write!(out, " --a{idx}: {};", hex(*rgb));
        }
        out
    }
}

/// A colour as the page paints it: one of the theme's (the default foreground
/// or background, or an ANSI colour 0–15), which the page resolves through its
/// palette variables, or a fixed RGB value that no theme remaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paint {
    Foreground,
    Background,
    Ansi(u8),
    Fixed((u8, u8, u8)),
}

impl Paint {
    fn of(color: vt100::Color, default: Paint) -> Paint {
        match color {
            vt100::Color::Default => default,
            vt100::Color::Idx(idx) if idx < 16 => Paint::Ansi(idx),
            // 16–255 are the xterm cube and grey ramp, the same in every theme.
            vt100::Color::Idx(idx) => Paint::Fixed(Palette::dark().indexed(idx)),
            vt100::Color::Rgb(r, g, b) => Paint::Fixed((r, g, b)),
        }
    }

    fn css(self) -> String {
        match self {
            Paint::Foreground => "var(--fg)".to_string(),
            Paint::Background => "var(--bg)".to_string(),
            Paint::Ansi(idx) => format!("var(--a{idx})"),
            Paint::Fixed(rgb) => hex(rgb),
        }
    }
}

/// A cell's text colour: a paint, or (SGR 2, dim) half way from one paint to
/// another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ink {
    Plain(Paint),
    Dim(Paint, Paint),
}

impl Ink {
    fn css(self) -> String {
        match self {
            Ink::Plain(paint) => paint.css(),
            // Two fixed colours mix to a fixed colour, written as hex.
            Ink::Dim(Paint::Fixed(fg), Paint::Fixed(bg)) => hex((
                ((u16::from(fg.0) + u16::from(bg.0)) / 2) as u8,
                ((u16::from(fg.1) + u16::from(bg.1)) / 2) as u8,
                ((u16::from(fg.2) + u16::from(bg.2)) / 2) as u8,
            )),
            Ink::Dim(fg, bg) => format!("color-mix(in srgb, {} 50%, {})", fg.css(), bg.css()),
        }
    }
}

/// How a frame is laid out on the page.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOptions {
    /// The page's default theme, and the one under `data-theme="dark"`.
    pub palette: Palette,
    /// The theme under `prefers-color-scheme: light` (without
    /// `data-theme="dark"`) and under `data-theme="light"`.
    pub light_palette: Palette,
    /// CSS font stack. The first family that is installed wins, so the
    /// maintainer doc names the one the committed images were made with.
    ///
    /// It is written into the page's `<style>` element, so a value holding
    /// anything but family names — letters, digits, spaces, commas, quotes,
    /// `-` and `_` — is replaced by [`DEFAULT_FONT_FAMILY`] rather than
    /// written: a `<` could close the element and a `;` or `}` could start a
    /// rule of its own. See [`font_family_or_default`].
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

/// The font stack the committed TUI images were made with.
pub const DEFAULT_FONT_FAMILY: &str = "\"DejaVu Sans Mono\", \"Liberation Mono\", monospace";

/// `stack` when it is made only of characters a list of font family names
/// needs, and [`DEFAULT_FONT_FAMILY`] otherwise. The check is an allowlist,
/// not an escape: CSS inside `<style>` has no escaping that also stops the
/// HTML parser closing the element, so the only safe value is one that holds
/// no markup or CSS punctuation at all.
pub fn font_family_or_default(stack: &str) -> &str {
    let safe = !stack.trim().is_empty()
        && stack
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | ',' | '"' | '\'' | '-' | '_'));
    if safe { stack } else { DEFAULT_FONT_FAMILY }
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            palette: Palette::dark(),
            light_palette: Palette::light(),
            font_family: DEFAULT_FONT_FAMILY.to_string(),
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
    fg: Ink,
    bg: Paint,
    bold: bool,
    italic: bool,
    underline: bool,
}

fn cell_style(cell: &vt100::Cell) -> CellStyle {
    let mut fg = Paint::of(cell.fgcolor(), Paint::Foreground);
    let mut bg = Paint::of(cell.bgcolor(), Paint::Background);
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    // Dim is half way to the background, which is how most terminals draw SGR 2.
    let fg = if cell.dim() {
        Ink::Dim(fg, bg)
    } else {
        Ink::Plain(fg)
    };
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

fn open_span(out: &mut String, style: &CellStyle) {
    let mut css = String::new();
    if style.fg != Ink::Plain(Paint::Foreground) {
        let _ = write!(css, "color:{};", style.fg.css());
    }
    if style.bg != Paint::Background {
        let _ = write!(css, "background:{};", style.bg.css());
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
/// every screen a docs image shows, and a stray block would be noise. Colours
/// the theme owns are `var(--fg)`, `var(--bg)` and `var(--a0)`–`var(--a15)`,
/// which [`render_page`] defines for both themes.
pub fn render_rows(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
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
            let style = cell_style(cell);
            if current != Some(style) {
                if current.is_some() {
                    out.push_str("</span>");
                }
                open_span(&mut out, &style);
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
///
/// The page is in [`RenderOptions::palette`] (dark) unless the viewer prefers a
/// light colour scheme or the root carries `data-theme="light"`, when it is in
/// [`RenderOptions::light_palette`]; `data-theme="dark"` keeps it dark on a
/// light-preferring viewer. The rasterizer sets both, so the image does not
/// depend on which one an engine honours.
pub fn render_page(screen: &vt100::Screen, title: &str, options: &RenderOptions) -> String {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>");
    push_escaped(&mut out, title);
    out.push_str("</title>\n<style>\n");
    let dark = options.palette.variables();
    let light = options.light_palette.variables();
    let _ = writeln!(out, ":root {{ color-scheme: dark; {dark} }}");
    let _ = writeln!(
        out,
        "@media (prefers-color-scheme: light) {{ :root:not([data-theme=\"dark\"]) {{ color-scheme: light; {light} }} }}"
    );
    let _ = writeln!(
        out,
        ":root[data-theme=\"light\"] {{ color-scheme: light; {light} }}"
    );
    out.push_str("html, body { margin: 0; padding: 0; background: var(--bg); }\n");
    let _ = writeln!(
        out,
        "#terminal {{ display: inline-block; padding: {pad}px; background: var(--bg); color: var(--fg); \
         font-family: {font}; font-size: {size}px; line-height: {lh}; \
         font-variant-ligatures: none; font-kerning: none; \
         -webkit-font-smoothing: antialiased; }}",
        pad = options.padding_px,
        font = font_family_or_default(&options.font_family),
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
    out.push_str(&render_rows(screen));
    out.push_str("</div>\n</body>\n</html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let axis =
            |v| std::num::NonZeroU16::new(v).expect("a test screen has non-zero rows and columns");
        let mut parser = vt100::Parser::new(axis(rows), axis(cols), 0);
        parser.process(bytes);
        parser
    }

    #[test]
    fn renders_one_div_per_row_padded_to_the_full_width() {
        let parser = screen(3, 5, b"hi");
        let rows = render_rows(parser.screen());
        assert_eq!(rows.matches("<div class=\"row\">").count(), 3);
        // Blank cells are spaces, so every row is exactly `cols` characters.
        assert!(rows.contains("<div class=\"row\"><span>hi   </span></div>"));
        assert!(rows.contains("<div class=\"row\"><span>     </span></div>"));
    }

    #[test]
    fn ansi_colours_are_the_themes_and_indexed_and_rgb_colours_are_fixed() {
        // SGR 31 (ANSI red), then 38;5;196 (cube red), then 38;2 (truecolour).
        let parser = screen(1, 3, b"\x1b[31mA\x1b[38;5;196mB\x1b[38;2;1;2;3mC");
        let rows = render_rows(parser.screen());
        assert!(rows.contains("color:var(--a1);"), "{rows}");
        assert!(rows.contains("color:#ff0000;"));
        assert!(rows.contains("color:#010203;"));
        let palette = Palette::default();
        assert_eq!(palette.indexed(16), (0, 0, 0));
        assert_eq!(palette.indexed(231), (255, 255, 255));
        assert_eq!(palette.indexed(232), (8, 8, 8));
        assert_eq!(palette.indexed(255), (238, 238, 238));
        // The cube and the ramp are no theme's: the light palette agrees.
        assert_eq!(Palette::light().indexed(196), palette.indexed(196));
    }

    #[test]
    fn background_colours_survive() {
        let parser = screen(1, 2, b"\x1b[44mX\x1b[48;5;236mY");
        let rows = render_rows(parser.screen());
        assert!(rows.contains("background:var(--a4);"), "{rows}");
        assert!(rows.contains("background:#303030;"), "{rows}");
    }

    #[test]
    fn inverse_swaps_foreground_and_background() {
        let parser = screen(1, 1, b"\x1b[7mX");
        let rows = render_rows(parser.screen());
        assert!(
            rows.contains("color:var(--bg);background:var(--fg);"),
            "{rows}"
        );
    }

    #[test]
    fn bold_italic_underline_and_dim_are_all_carried() {
        let parser = screen(
            1,
            5,
            b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[4mU\x1b[0m\x1b[2mD\x1b[0m\x1b[2;38;2;200;200;200;48;2;0;0;0mF",
        );
        let rows = render_rows(parser.screen());
        assert!(rows.contains("<span style=\"font-weight:700;\">B</span>"));
        assert!(rows.contains("<span style=\"font-style:italic;\">I</span>"));
        assert!(rows.contains("<span style=\"text-decoration:underline;\">U</span>"));
        // Dim is half way from the foreground to the background: mixed by the
        // page when either is the theme's, computed here when both are fixed.
        assert!(
            rows.contains(
                "<span style=\"color:color-mix(in srgb, var(--fg) 50%, var(--bg));\">D</span>"
            ),
            "{rows}"
        );
        assert!(
            rows.contains("<span style=\"color:#646464;background:#000000;\">F</span>"),
            "{rows}"
        );
    }

    #[test]
    fn same_styled_cells_share_one_span() {
        let parser = screen(1, 6, b"\x1b[32mgreen\x1b[0m!");
        let rows = render_rows(parser.screen());
        assert_eq!(rows.matches("<span").count(), 2);
    }

    #[test]
    fn markup_characters_are_escaped() {
        let parser = screen(1, 8, b"<a&b>\"");
        let rows = render_rows(parser.screen());
        assert!(rows.contains("&lt;a&amp;b&gt;&quot;"));
        assert!(!rows.contains("<a"));
    }

    #[test]
    fn a_wide_character_is_emitted_once_at_two_columns() {
        let parser = screen(1, 4, "日x".as_bytes());
        let rows = render_rows(parser.screen());
        assert_eq!(rows.matches('日').count(), 1);
        assert!(rows.contains("<span class=\"wide\">日</span>x "));
    }

    #[test]
    fn box_drawing_passes_through_untouched() {
        let parser = screen(1, 3, "┌─┐".as_bytes());
        let rows = render_rows(parser.screen());
        assert!(rows.contains("┌─┐"));
    }

    /// WCAG 2 contrast ratio of two colours.
    fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        let channel = |v: u8| {
            let c = f64::from(v) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let lum = |(r, g, b): (u8, u8, u8)| {
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn the_page_defines_both_themes_dark_by_default() {
        let parser = screen(1, 4, b"\x1b[32mok");
        let options = RenderOptions::default();
        let page = render_page(parser.screen(), "t", &options);
        let dark = options.palette.variables();
        let light = options.light_palette.variables();
        assert_ne!(dark, light);
        assert!(
            page.contains(&format!(":root {{ color-scheme: dark; {dark} }}")),
            "{page}"
        );
        assert!(page.contains(&format!(
            "@media (prefers-color-scheme: light) {{ :root:not([data-theme=\"dark\"]) {{ color-scheme: light; {light} }} }}"
        )));
        assert!(page.contains(&format!(
            ":root[data-theme=\"light\"] {{ color-scheme: light; {light} }}"
        )));
        // Every theme colour the page uses is one of the variables it defines.
        for name in ["--fg", "--bg", "--a2"] {
            assert!(dark.contains(&format!("{name}: ")), "{name}");
            assert!(light.contains(&format!("{name}: ")), "{name}");
        }
        assert!(page.contains("color:var(--a2);"));
        // The cells name the variable, never one theme's value.
        let rows = render_rows(parser.screen());
        assert!(!rows.contains(&hex(options.palette.ansi[2])), "{rows}");
        assert!(
            !rows.contains(&hex(options.light_palette.ansi[2])),
            "{rows}"
        );
    }

    #[test]
    fn the_light_palette_is_light_and_legible() {
        let light = Palette::light();
        let dark = Palette::dark();
        assert!(
            contrast(light.background, (255, 255, 255)) < 1.1,
            "a light ground"
        );
        assert!(contrast(dark.background, (0, 0, 0)) < 1.6, "a dark ground");
        assert!(contrast(light.foreground, light.background) >= 7.0);
        // All sixteen ANSI colours read at 4.5:1 or more, white (7) and bright
        // white (15) included: the TUI draws text in both — the project-trust
        // prompt's labels in white and its command values in bright white
        // (`src/trust_prompt.rs`) — and a light image that hides them hides
        // what the screen asks the user to approve (PR #2491 review).
        for idx in 0..16 {
            let ratio = contrast(light.ansi[idx], light.background);
            assert!(
                ratio >= 4.5,
                "ANSI {idx} is {ratio:.2}:1 on the light ground"
            );
        }
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
        let bytes = b"\x1b[1;36mdot-agent-deck\x1b[0m \xe2\x94\x80 3 agent(s)\r\n\x1b[33m\xe2\x97\x8f\x1b[0m Working";
        let a = screen(4, 40, bytes);
        let b = screen(4, 40, bytes);
        let options = RenderOptions::default();
        assert_eq!(
            render_page(a.screen(), "t", &options),
            render_page(b.screen(), "t", &options)
        );
    }

    #[test]
    fn a_font_family_cannot_close_the_style_element_or_add_a_rule() {
        let parser = screen(1, 4, b"x");
        for hostile in [
            "monospace</style><script>alert(1)</script>",
            "monospace; } body { display: none",
            "monospace\n</STYLE>",
            "",
        ] {
            let options = RenderOptions {
                font_family: hostile.to_string(),
                ..RenderOptions::default()
            };
            let page = render_page(parser.screen(), "t", &options);
            assert_eq!(page.matches("</style>").count(), 1, "{hostile:?}");
            assert!(
                !page.to_ascii_lowercase().contains("<script"),
                "{hostile:?}"
            );
            assert!(!page.contains("display: none"), "{hostile:?}");
            assert!(
                page.contains(&format!("font-family: {DEFAULT_FONT_FAMILY};")),
                "{hostile:?}"
            );
        }
        // A genuine stack is written as given.
        let options = RenderOptions {
            font_family: "'Fira Code', monospace".to_string(),
            ..RenderOptions::default()
        };
        let page = render_page(parser.screen(), "t", &options);
        assert!(page.contains("font-family: 'Fira Code', monospace;"));
    }
}
