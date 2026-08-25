//! Reduce a pane's raw PTY scrollback to the plain text its screen is showing.
//!
//! Issue #686: the daemon arms a watch on a delegated worker and, when the
//! worker emits no agent event inside the window, writes a notice into the
//! orchestrator's pane. That notice used to assert a cause it had not checked —
//! that the prompt was probably never delivered — while the pane's own bytes
//! were sitting in the same registry that armed the watch. Reporting those
//! bytes turns a guess into an observation, and it does so by the SYMPTOM (a
//! pane that emitted nothing) rather than by the agent's identity, which the
//! deck frequently cannot determine at all: `AgentType::from_command` cannot see
//! through a `devbox run …` / `mise` / `npm run` launcher, and the learned badge
//! comes from a hook event, which is precisely what is missing here.
//!
//! The raw bytes are not usable as text. They carry ANSI cursor addressing,
//! erases and colour, so a full-screen agent TUI's scrollback bears no
//! resemblance to what is on screen. Feeding them through a `vt100::Parser` —
//! the same parser the TUI renders panes with — and reading the resulting grid
//! is what makes "what the pane is showing" a well-defined question.
//!
//! **Cost.** The replay is O(scrollback), and the daemon caps a pane's ring at
//! `SCROLLBACK_CAP_BYTES` (1 MiB), so the worst case is bounded rather than
//! open-ended: measured at **26–29 ms** for a full 1 MiB of escape-laden output
//! in a release build. That is paid inline on the watch's own detached task
//! rather than handed to `spawn_blocking`, because it happens at most once per
//! delegation and only after that delegation has already been silent for the
//! whole response window — a thread hop to save a one-off 26 ms on a path
//! gated behind 30 s of waiting buys nothing.

use std::panic::AssertUnwindSafe;

use crate::embedded_pane::parser_init_dims;

/// How many of the screen's trailing non-blank rows a caller may ask for.
///
/// Not a style preference: the text this module returns is inlined into a
/// single-line notice written into a live agent's input (see
/// `state::compose_delegate_silence_notice`), so every row costs budget in
/// something a person and an LLM both have to read. Six rows is enough to carry
/// an agent's ready prompt plus its hint line, or a small modal and its choices,
/// which are the shapes that actually answer "why is this pane quiet?".
pub const MAX_REPORTED_ROWS: usize = 6;

/// Longest run of one repeated non-alphanumeric character kept intact.
///
/// Box-drawing rules and separator bars are the bulk of a bordered TUI's
/// characters and carry no information once the rows are joined into one line —
/// a 78-column `───────…` would eat the whole character budget of the notice and
/// push out the words. Collapsing to three keeps the visual cue that a rule was
/// there without paying for its width. Alphanumerics are deliberately exempt so
/// nothing an agent actually wrote is compressed.
const MAX_REPEATED_RULE_RUN: usize = 3;

/// The trailing non-blank rows of the screen `snapshot` produces at
/// `rows`x`cols`, oldest first, at most `max_lines` of them.
///
/// `rows`/`cols` are the PTY dims the bytes were WRITTEN at — parsing at any
/// other geometry re-wraps the content and can turn a readable screen into
/// nonsense — and they pass through [`parser_init_dims`] so a zero or
/// out-of-range pair falls back to 24x80 instead of building a parser that
/// panics on its first byte.
///
/// Blank rows are dropped rather than preserved: a full-screen TUI pads its grid
/// out to the bottom, so the literal last rows of the screen are usually empty
/// and reporting them would report nothing.
pub fn visible_tail_lines(snapshot: &[u8], rows: u16, cols: u16, max_lines: usize) -> Vec<String> {
    if snapshot.is_empty() || max_lines == 0 {
        return Vec::new();
    }
    let (rows, cols) = parser_init_dims(rows, cols);
    // `parser_init_dims` admits a 1-row / 1-col parser and vt100 0.16.2
    // underflows in `col_wrap` the moment text wraps in one that short —
    // `embedded_pane::seam_pane` contains the same guard for the same reason.
    // Here the stakes are lower but the containment still matters: this runs on
    // a daemon task whose only job is to write a diagnostic, and a diagnostic
    // must never be able to take the daemon down. On a contained panic the
    // caller gets no lines and the notice says the pane rendered nothing, which
    // is the honest answer when the screen could not be reconstructed.
    let parsed = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(snapshot);
        parser
            .screen()
            .rows(0, cols)
            .map(|row| collapse_rules(row.trim()))
            .collect::<Vec<_>>()
    }));
    let Ok(lines) = parsed else {
        tracing::warn!(
            rows,
            cols,
            "vt100 parser panicked rendering a pane snapshot for a daemon notice; reporting the \
             pane as blank. Known vt100 0.16.2 edge case in a very short pane."
        );
        return Vec::new();
    };
    let mut tail: Vec<String> = lines
        .into_iter()
        .rev()
        .filter(|line| !line.is_empty())
        .take(max_lines)
        .collect();
    tail.reverse();
    tail
}

/// Shrink runs of one repeated non-alphanumeric character to
/// [`MAX_REPEATED_RULE_RUN`], leaving everything else byte-for-byte alone.
fn collapse_rules(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run_char = None;
    let mut run_len = 0usize;
    for c in line.chars() {
        if Some(c) == run_char {
            run_len += 1;
        } else {
            run_char = Some(c);
            run_len = 1;
        }
        if c.is_alphanumeric() || run_len <= MAX_REPEATED_RULE_RUN {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_tail_lines_reports_the_screen_not_the_escape_bytes() {
        // A cursor-addressed redraw: the bytes contain "stale" but the SCREEN
        // does not, which is the whole reason this goes through a parser rather
        // than a regex over the scrollback.
        let snapshot = b"stale banner\r\n\x1b[H\x1b[2Jready prompt\r\n";
        assert_eq!(
            visible_tail_lines(snapshot, 24, 80, MAX_REPORTED_ROWS),
            vec!["ready prompt".to_string()]
        );
    }

    #[test]
    fn visible_tail_lines_keeps_the_last_rows_in_order() {
        let snapshot = b"one\r\ntwo\r\nthree\r\nfour\r\n";
        assert_eq!(
            visible_tail_lines(snapshot, 24, 80, 2),
            vec!["three".to_string(), "four".to_string()],
            "the tail must stay in reading order, not reversed"
        );
    }

    #[test]
    fn visible_tail_lines_skips_the_blank_padding_of_a_full_screen_grid() {
        // Row 1 of a 24-row grid; rows 2..24 are blank padding. Reporting the
        // literal last rows would report nothing at all.
        let snapshot = b"Ask the agent to do anything\r\n";
        assert_eq!(
            visible_tail_lines(snapshot, 24, 80, MAX_REPORTED_ROWS),
            vec!["Ask the agent to do anything".to_string()]
        );
    }

    #[test]
    fn visible_tail_lines_is_empty_for_a_pane_that_rendered_nothing() {
        assert!(visible_tail_lines(b"", 24, 80, MAX_REPORTED_ROWS).is_empty());
        assert!(
            visible_tail_lines(b"   \r\n\r\n", 24, 80, MAX_REPORTED_ROWS).is_empty(),
            "whitespace-only rows are not content"
        );
    }

    #[test]
    fn visible_tail_lines_survives_unusable_dims_instead_of_panicking() {
        // (0, 0) is the legacy-daemon shape `parser_init_dims` rewrites to
        // 24x80; without that guard this builds a parser that panics on its
        // first byte.
        assert_eq!(
            visible_tail_lines(b"content\r\n", 0, 0, MAX_REPORTED_ROWS),
            vec!["content".to_string()]
        );
    }

    #[test]
    fn collapse_rules_shrinks_borders_but_never_words() {
        assert_eq!(
            collapse_rules("\u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}"),
            "\u{256d}\u{2500}\u{2500}\u{2500}\u{256e}"
        );
        assert_eq!(collapse_rules("======== done"), "=== done");
        assert_eq!(
            collapse_rules("aaaaaa bbbb"),
            "aaaaaa bbbb",
            "alphanumeric runs are content and must survive intact"
        );
    }
}
