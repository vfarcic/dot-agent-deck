//! The deck's `u16` grid geometry at the vt100 boundary.
//!
//! vt100 0.18 takes and returns a grid's rows and columns as `NonZeroU16`,
//! while the deck carries them as `u16` end to end (the daemon protocol, the
//! PTY resize path, ratatui's `Rect`). These three functions are the one place
//! the two meet, so call sites keep reading in `u16`.
//!
//! A zero axis was never a valid vt100 grid (0.16 underflowed building one, the
//! hazard issue #363 guarded against). Here it is raised to 1, the smallest
//! grid vt100 accepts; callers that need a better fallback, such as
//! `embedded_pane::parser_init_dims`, apply it before calling in.

use std::num::NonZeroU16;

fn axis(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v).unwrap_or(NonZeroU16::MIN)
}

/// `vt100::Parser::new` with `u16` dimensions.
pub fn parser(rows: u16, cols: u16, scrollback_len: usize) -> vt100::Parser {
    vt100::Parser::new(axis(rows), axis(cols), scrollback_len)
}

/// `vt100::Screen::size` as `(rows, cols)` in `u16`.
pub fn size(screen: &vt100::Screen) -> (u16, u16) {
    let (rows, cols) = screen.size();
    (rows.get(), cols.get())
}

/// `vt100::Screen::set_size` with `u16` dimensions.
pub fn set_size(screen: &mut vt100::Screen, rows: u16, cols: u16) {
    screen.set_size(axis(rows), axis(cols));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_axis_becomes_one() {
        assert_eq!(size(parser(0, 0, 0).screen()), (1, 1));
        let mut p = parser(24, 80, 0);
        set_size(p.screen_mut(), 0, 40);
        assert_eq!(size(p.screen()), (1, 40));
    }

    #[test]
    fn non_zero_dimensions_pass_through() {
        let mut p = parser(24, 80, 0);
        assert_eq!(size(p.screen()), (24, 80));
        set_size(p.screen_mut(), 40, 120);
        assert_eq!(size(p.screen()), (40, 120));
    }
}
