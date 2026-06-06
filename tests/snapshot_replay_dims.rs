//! PRD #104 M4 — failure-mode reproducer for the snapshot-replay
//! dimension scramble.
//!
//! The bug: on reconnect, `EmbeddedPaneController::hydrate_from_daemon`
//! built every local vt100 parser at the hard-coded 24×80 default before
//! feeding the daemon's scrollback snapshot through it. Snapshots
//! emitted at a wider PTY geometry (cursor-position escapes referencing
//! columns past 80, wraps that wouldn't wrap at 80, full-screen redraws
//! sized for an N-column screen) got mis-parsed: cursor sequences
//! clamped to col 79, spurious wraps appeared at col 80, and the parser
//! baked the resulting cell layout permanently into its scrollback
//! (vt100 does not reflow on resize). Scrolled-back rows showed up as
//! overlapping text and narrow vertical strips on the right edge of the
//! pane.
//!
//! The fix (M1+M2): the daemon now echoes its current PTY (rows, cols)
//! via `AgentRecord`, and `parser_init_dims` propagates those values to
//! the parser constructor. M3 closes the residual case where a single
//! snapshot spans multiple dimension epochs by clearing the daemon-side
//! scrollback on every resize ioctl.
//!
//! This test pins the M2 contract: given a daemon-supplied dim pair
//! that exceeds 80 columns, the parser the hydration path constructs
//! must preserve cell content at columns past 80. Pre-PRD this test
//! does not compile (`parser_init_dims` did not exist) and the
//! hard-coded 24×80 site would corrupt the same sentinel — both are
//! captured below so future regressions surface as a test failure
//! rather than a "did the user notice the scramble" question.

use dot_agent_deck::embedded_pane::parser_init_dims;

/// The recognizable byte string we plant at col 100. Eight bytes wide
/// so it's a single contiguous run on any sane terminal; ASCII so
/// vt100 doesn't have to make wide-glyph judgement calls.
const SENTINEL: &str = "SENTINEL";

/// Read the contents of cells `[col_start, col_start + len)` on `row`
/// (both 0-indexed) and return their concatenation. Empty cells
/// contribute the empty string, so a partially-clipped sentinel shows
/// up as a prefix.
fn read_row_slice(parser: &vt100::Parser, row: u16, col_start: u16, len: u16) -> String {
    let screen = parser.screen();
    let mut out = String::new();
    for col in col_start..col_start + len {
        if let Some(cell) = screen.cell(row, col) {
            out.push_str(cell.contents());
        }
    }
    out
}

#[test]
fn hydration_at_wide_dims_preserves_sentinel_past_col_80() {
    // Simulate the daemon returning an `AgentRecord` with rows=40,
    // cols=120 — a wide-PTY agent. `parser_init_dims` is the seam the
    // hydration path uses to convert those wire values into the
    // (rows, cols) passed to `vt100::Parser::new` inside
    // `wire_stream_pane`.
    let (rows, cols) = parser_init_dims(40, 120);
    assert_eq!(
        (rows, cols),
        (40, 120),
        "PRD #104 M2 contract: hydration must propagate daemon-reported dims"
    );

    let mut parser = vt100::Parser::new(rows, cols, 10_000);
    assert_eq!(
        parser.screen().size(),
        (40, 120),
        "parser must be constructed at the resolved dims so snapshot \
         bytes are parsed at the same geometry the daemon emitted them at"
    );

    // CSI cursor position is 1-indexed: row 10, col 101 lands the
    // sentinel at 0-indexed (row 9, col 100..108). At 24×80 this
    // sequence clamps to (row 9, col 79) and SENTINEL overprints col
    // 79 byte by byte — the exact failure the PRD describes.
    parser.process(b"\x1b[10;101HSENTINEL");

    assert_eq!(
        read_row_slice(&parser, 9, 100, 8),
        SENTINEL,
        "wide-dim hydration must preserve the sentinel at col 100 — \
         a 24×80 parser would clamp it to col 79 and corrupt scrollback"
    );
}

#[test]
fn parser_at_24x80_demonstrates_pre_prd_corruption() {
    // The control case: a vt100 parser sized at the pre-PRD 24×80
    // default. This is what `wire_stream_pane` used to construct
    // regardless of the daemon's actual PTY dims (see the
    // `// PRD #76 M2.15` comment block this PRD replaced). The same
    // bytes that placed an intact SENTINEL at col 100 above must NOT
    // produce a recognizable SENTINEL string anywhere on this
    // parser's screen — that's the regression we just closed.
    let mut parser = vt100::Parser::new(24, 80, 10_000);
    parser.process(b"\x1b[10;101HSENTINEL");

    // Scan every cell across every row for the literal sentinel byte
    // string. With cursor clamping at col 79 and per-byte overprint
    // semantics, no row will carry the full 8-byte run.
    let screen = parser.screen();
    for row in 0..24u16 {
        let row_text = read_row_slice(&parser, row, 0, 80);
        assert!(
            !row_text.contains(SENTINEL),
            "pre-PRD 24×80 parser unexpectedly preserved sentinel — \
             the corruption this PRD fixes should have clipped it. \
             row={row} contents={row_text:?}"
        );
        let _ = screen; // keep the screen borrow scoped for clarity
    }
}

#[test]
fn parser_init_dims_falls_back_on_legacy_daemon_zero_values() {
    // Forward-compat: a daemon predating PRD #104 omits rows/cols on
    // the wire. `#[serde(default)]` decodes those as 0, and
    // `parser_init_dims` must fall back to the historical 24×80
    // placeholder rather than passing zero to vt100 (which has subtle
    // edge cases at zero dims).
    assert_eq!(parser_init_dims(0, 0), (24, 80));
}
