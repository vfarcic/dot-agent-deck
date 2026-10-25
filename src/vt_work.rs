//! PRD #1666 part 3, round 5-wake (Q25): what a byte costs a `vt100` parser,
//! counted before the parser is given it.
//!
//! The release scanner and the doorbell's evidence parser run inside the
//! PTY reader's push, under the pane's output lock. Most of what an agent
//! prints costs them a few dozen nanoseconds a byte, but `vt100` does some
//! operations over the whole grid: an erase-display clears every cell, so a
//! pane at 512 × 512 that prints screen-clears without pause makes each one
//! clear 262 144 cells. Both parsers therefore work to a budget counted here,
//! in **work units**, one unit for each cell an operation can write or move
//! and [`BYTE_UNITS`] for every byte's own parse and check.
//!
//! The count is of work, not of time, so a loaded machine does not make a
//! normal launch look expensive, and it is taken **before** the byte reaches
//! the parser: [`MeteredParser::feed_byte`] prices the byte with a shadow
//! `vte` parser, the tokenizer `vt100` itself runs on, and refuses it when the
//! price would take the caller past its limit. A refused byte is never
//! parsed, so no push does more than its limit's worth of work.
//!
//! The prices are upper bounds for `vt100` 0.18.1's `screen.rs` and
//! `grid.rs`, operation by operation, with `C` the grid's columns, `R` its
//! rows and `S = C + R`, the cost of one scroll (a row reset and a rotation of
//! the row list):
//!
//! - a printed character: 1, plus `S` when the cursor is within two columns of
//!   the row's end, where printing can wrap and scroll;
//! - line feed, vertical tab, form feed, reverse index: `S`;
//! - full reset (`ESC c`): `16 × R × C`, since a new screen allocates every
//!   row and the old one frees them;
//! - erase in display (`CSI J`, `CSI ? J`): `R × C + 4 × C`, the rows it
//!   clears whole and the cursor's row, which it erases cell by cell;
//! - erase in line and erase characters (`CSI K`, `CSI ? K`, `X`): `4 × C`,
//!   erased cell by cell; insert and delete characters (`@`, `P`): `C`;
//! - insert and delete lines, scroll up and down (`CSI L`, `M`, `S`, `T`, `n`
//!   times): `min(n, R) × C + R`;
//! - repeat (`CSI n b`): `n` printed characters, each at [`BYTE_UNITS`], plus
//!   a scroll for every row they can fill;
//! - the alternate screen (`CSI ? 47 h`, `CSI ? 1049 h`): `R × C` for each
//!   such parameter, since a sequence may carry up to 32, and `16 × R × C`
//!   more for the first one after the parser starts or is reset, which
//!   allocates the alternate grid;
//! - a CSI sequence's parameters: 1 each, for the list `vt100` builds of them;
//! - everything else `vt100` does (cursor moves, modes, attributes, titles):
//!   nothing beyond the byte.
//!
//! The units are a model, so what a unit costs in time is measured, not
//! derived: `src/agent_pty_scanner_budget.rs` replays the recorded launches
//! and a pathological trace and prints nanoseconds per unit beside the
//! limits.

/// Work units charged for every byte: the parse step, and for the release
/// scanner the check of the cursor's row. Calibrated against the recorded
/// launches, whose bytes cost about this many cell clears each.
pub const BYTE_UNITS: u64 = 64;

/// What one dispatched action costs, given the grid and the cursor before the
/// byte that dispatches it.
struct Pricing {
    rows: u64,
    cols: u64,
    col: u64,
    units: u64,
    /// Whether `vt100` has allocated the alternate grid: it does at the
    /// first switch to it, and a full reset builds a screen without one.
    alternate_allocated: bool,
}

impl Pricing {
    fn scroll(&self) -> u64 {
        self.cols + self.rows
    }

    fn grid(&self) -> u64 {
        self.rows * self.cols
    }

    fn lines(&self, n: u64) -> u64 {
        n.min(self.rows) * self.cols + self.rows
    }
}

fn first_param(params: &vte::Params, default: u16) -> u64 {
    let first = params.iter().next().map_or(0, |p| *p.first().unwrap_or(&0));
    u64::from(if first == 0 { default } else { first })
}

impl vte::Perform for Pricing {
    fn print(&mut self, _c: char) {
        self.units += 1;
        if self.col + 2 > self.cols {
            self.units += self.scroll();
        }
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, 10..=12) {
            self.units += self.scroll();
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'M' => self.units += self.scroll(),
            b'c' => {
                self.units += 16 * self.grid() + self.rows;
                self.alternate_allocated = false;
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        self.units += params.len() as u64;
        match (intermediates.first(), action) {
            (None | Some(b'?'), 'J') => self.units += self.grid() + 4 * self.cols,
            (None | Some(b'?'), 'K') | (None, 'X') => self.units += 4 * self.cols,
            (None, '@' | 'P') => self.units += self.cols,
            (None, 'L' | 'M' | 'S' | 'T') => self.units += self.lines(first_param(params, 1)),
            (None, 'b') => {
                let n = first_param(params, 1);
                // A wide character fills a row every `C / 2` repeats.
                let rows_filled = 2 * n / self.cols.max(1) + 1;
                self.units += n * BYTE_UNITS + rows_filled * self.scroll();
            }
            (Some(b'?'), 'h' | 'l') => {
                let screens = params.iter().filter(|p| matches!(p, [47] | [1049])).count() as u64;
                self.units += screens * self.grid();
                if screens > 0 && action == 'h' && !self.alternate_allocated {
                    self.units += 16 * self.grid();
                    self.alternate_allocated = true;
                }
            }
            _ => {}
        }
    }
}

/// Prices bytes for one `vt100` parser. It must see exactly the bytes that
/// parser sees, in the same order, so that its tokenizer stays in step.
pub struct WorkMeter {
    shadow: vte::Parser,
    alternate_allocated: bool,
}

impl Default for WorkMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkMeter {
    pub fn new() -> Self {
        Self {
            shadow: vte::Parser::new(),
            alternate_allocated: false,
        }
    }

    /// The units `byte` will cost the parser whose screen is `screen`, which
    /// has not been given it yet. Advances the shadow tokenizer past it.
    pub fn price(&mut self, byte: u8, screen: &vt100::Screen) -> u64 {
        let (rows, cols) = crate::vt_grid::size(screen);
        let (_, col) = screen.cursor_position();
        let mut pricing = Pricing {
            rows: u64::from(rows),
            cols: u64::from(cols),
            col: u64::from(col),
            units: BYTE_UNITS,
            alternate_allocated: self.alternate_allocated,
        };
        self.shadow.advance(&mut pricing, &[byte]);
        self.alternate_allocated = pricing.alternate_allocated;
        pricing.units
    }
}

/// A `vt100` parser that is fed one byte at a time, each byte priced first.
pub struct MeteredParser {
    parser: vt100::Parser,
    meter: WorkMeter,
    spent: u64,
}

impl MeteredParser {
    /// [`crate::vt_grid::parser`], metered.
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self {
            parser: crate::vt_grid::parser(rows, cols, scrollback_len),
            meter: WorkMeter::new(),
            spent: 0,
        }
    }

    /// Feed `byte` if its price leaves `spent()` at or under `limit`, and
    /// answer whether it was fed. A byte refused leaves the parser and the
    /// count as they were; the meter has moved past it, so a caller that is
    /// refused stops feeding this parser.
    pub fn feed_byte(&mut self, byte: u8, limit: u64) -> bool {
        let price = self.meter.price(byte, self.parser.screen());
        let after = self.spent.saturating_add(price);
        if after > limit {
            return false;
        }
        self.spent = after;
        self.parser.process(&[byte]);
        true
    }

    /// The units fed so far.
    pub fn spent(&self) -> u64 {
        self.spent
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn screen_mut(&mut self) -> &mut vt100::Screen {
        self.parser.screen_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price_of(bytes: &[u8], rows: u16, cols: u16) -> u64 {
        let mut parser = MeteredParser::new(rows, cols, 0);
        for &b in bytes {
            assert!(parser.feed_byte(b, u64::MAX));
        }
        parser.spent()
    }

    #[test]
    fn plain_text_costs_its_bytes() {
        assert_eq!(price_of(b"hello", 24, 80), 5 * (BYTE_UNITS + 1));
    }

    #[test]
    fn an_erase_display_costs_the_grid() {
        let base = price_of(b"\x1b[2", 512, 512);
        let whole = price_of(b"\x1b[2J", 512, 512);
        assert_eq!(whole - base, BYTE_UNITS + 1 + 512 * 512 + 4 * 512);
    }

    #[test]
    fn a_line_feed_and_a_print_at_the_row_end_cost_a_scroll() {
        assert_eq!(price_of(b"\n", 10, 20), BYTE_UNITS + 30);
        // CUP to the last column, then print.
        let to_end = price_of(b"\x1b[1;20H", 10, 20);
        assert_eq!(
            price_of(b"\x1b[1;20Hx", 10, 20) - to_end,
            BYTE_UNITS + 1 + 30
        );
    }

    #[test]
    fn every_alternate_screen_parameter_costs_the_grid() {
        let one = price_of(b"\x1b[?1049h", 100, 100);
        assert!(one > 17 * 100 * 100, "the first switch allocates the grid");
        // After the first, each switch costs the grid once.
        let mut parser = MeteredParser::new(100, 100, 0);
        for &b in b"\x1b[?1049h" {
            parser.feed_byte(b, u64::MAX);
        }
        let first = parser.spent();
        for &b in b"\x1b[?1049;47;1049h" {
            parser.feed_byte(b, u64::MAX);
        }
        assert!(parser.spent() - first >= 3 * 100 * 100);
        assert!(parser.spent() - first < 17 * 100 * 100);
        // A mode that is not a screen switch costs its bytes and parameter.
        assert!(price_of(b"\x1b[?25h", 100, 100) < 10 * BYTE_UNITS);
    }

    #[test]
    fn a_repeat_is_charged_for_every_character_and_row_it_fills() {
        let base = price_of(b"x\x1b[65535", 4096, 64);
        let rep = price_of(b"x\x1b[65535b", 4096, 64);
        assert!(rep - base >= 65_535 * BYTE_UNITS + (2 * 65_535 / 64) * (4096 + 64));
    }

    #[test]
    fn a_refused_byte_is_never_parsed() {
        let mut parser = MeteredParser::new(512, 512, 0);
        for &b in b"ab\x1b[2" {
            assert!(parser.feed_byte(b, 10_000));
        }
        let spent = parser.spent();
        assert!(
            !parser.feed_byte(b'J', 10_000),
            "the erase is over the limit"
        );
        assert_eq!(parser.spent(), spent);
        assert_eq!(
            parser.screen().contents().trim(),
            "ab",
            "nothing was erased"
        );
    }

    /// The shadow tokenizer is `vt100`'s own: one `vte` in the lock file.
    #[test]
    fn the_meter_and_vt100_share_one_vte() {
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock"))
            .expect("Cargo.lock");
        let vtes = lock.matches("\nname = \"vte\"\n").count();
        assert_eq!(
            vtes, 1,
            "one vte version, so the meter tokenizes as vt100 does"
        );
    }
}
