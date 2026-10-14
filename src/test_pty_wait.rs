//! Content-keyed waits for a `/bin/cat` PTY byte-observation target — issues
//! #850, #851, #892 and #1132.
//!
//! A unit test that writes bytes at a real PTY and then asserts they arrived
//! needs an instant that is provably AFTER the write. `src/spawn.rs` learned
//! that the expensive way: every such assertion there used to follow a fixed
//! 75 ms sleep, which is a bet that a PTY round trip beats a constant — a bet a
//! contended runner loses, and the fast tier has no retries, so losing it reds
//! the required `build` job. Issues #850/#851/#892 replaced the bets with the
//! waits below, keyed on CONTENT and bounded only by a diagnostic deadline, so
//! load makes them slower rather than wrong.
//!
//! **This module exists because those waits had to leave `spawn.rs` to be
//! reused, and that is the whole decision issue #1132 was blocked on.** They
//! were private to `spawn.rs`'s own `#[cfg(test)] mod tests`, so
//! `prompt/pane-input/032` (`src/ui.rs`) and `scheduler/idle-worker/015`
//! (`src/state.rs`) — the same shape, against the same `/bin/cat` target, with
//! the same 75 ms sleeps — could not call them and were left carrying the bet.
//! The two rejected alternatives were copying the helpers into each test module
//! (three divergent copies of a fix whose subtlety is the reason it took three
//! issues to get right) and wrapping a short-sleep poll in `tokio::time::timeout`
//! at each site (the same poll, re-derived per caller, with none of the
//! diagnostics). A crate-level `#[cfg(test)]` module is the arrangement
//! `test_temp` and `test_isolation` already established for test-only support
//! that more than one module needs.
//!
//! Test-only and never part of the shipped library: `lib.rs` declares it under
//! `#[cfg(test)]`, so it is compiled for the lib target's own unit tests and
//! nowhere else.
//!
//! # What the target is, and why these counts hold
//!
//! Every caller spawns a bare `/bin/cat` through
//! [`crate::agent_pty::AgentPtyRegistry`] and reads the accumulated bytes back
//! with `snapshot`. Nothing else can reach that PTY — there is no wrapper in
//! front of it — so the buffer holds exactly two writers' output: the line
//! discipline's echo of what was typed, and `cat`'s copy of each line once a
//! terminator completes it. Every count below is read off that fact rather than
//! predicted from a platform's behaviour.

use crate::agent_pty::AgentPtyRegistry;
use std::time::{Duration, Instant};

/// [`line_ends`] with the platform choice made explicit, so the ConPTY branch is
/// testable on every platform and the Unix one stays byte-for-byte what it was.
fn line_ends_reading_cursor_jumps(bytes: &[u8], cursor_jumps: bool) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut row: usize = 1;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"\r\n") {
            at += 2;
            ends.push(at);
            row += 1;
            continue;
        }
        if cursor_jumps && let Some((target_row, length)) = cursor_position(&bytes[at..]) {
            at += length;
            for _ in row..target_row.max(row) {
                ends.push(at);
            }
            row = target_row;
            continue;
        }
        at += 1;
    }
    ends
}

/// A CUP sequence at the start of `bytes` — `ESC[H`, `ESC[<row>H` or
/// `ESC[<row>;<col>H` — as its 1-based target row and its length in bytes.
fn cursor_position(bytes: &[u8]) -> Option<(usize, usize)> {
    let params = bytes.strip_prefix(b"\x1b[")?;
    let end = params
        .iter()
        .position(|byte| !(byte.is_ascii_digit() || *byte == b';'))?;
    if params[end] != b'H' {
        return None;
    }
    let row_digits = params[..end]
        .split(|byte| *byte == b';')
        .next()
        .unwrap_or_default();
    let row = std::str::from_utf8(row_digits)
        .ok()?
        .parse::<usize>()
        .unwrap_or(1)
        .max(1);
    Some((row, 2 + end + 1))
}

/// Block until `needle` is visible on the target, or fail with the buffer
/// that was actually observed.
///
/// Content-keyed for the same reason `spawn.rs`'s
/// `wait_for_detached_payload_echo` is:
/// the caller needs an instant that is provably AFTER a specific write, and
/// "the buffer grew" only proves that if nothing else can put a byte there.
pub(crate) async fn wait_for_echo_bytes(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    needle: &[u8],
) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let snapshot = registry.snapshot(agent_id).expect("byte target snapshot");
        if snapshot
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {:?} to reach the target; snapshot={:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&snapshot)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buffer `scheduler/dispatch/016` held when it timed out on
    /// `build-windows` (PR #1474's first run), verbatim: four payload lines
    /// drawn as `\r\n`, then two submit-only probes whose four blank lines
    /// ConPTY drew as two cursor jumps.
    const CONPTY_SNAPSHOT: &[u8] = b"\x1b[?9001h\x1b[?1004h\x1b[?25l\x1b[2J\x1b[m\x1b[H\x1b]0;C:\\Windows\\system32\\more.com\x07\x1b[?25h\x1b[79XISSUE-666-ARMED-THIRD-PAYLOAD\r\nISSUE-666-ARMED-THIRD-PAYLOAD\r\nISSUE-666-ARMED-THIRD-PAYLOAD\r\nISSUE-666-ARMED-THIRD-PAYLOAD\r\n\x1b[?25l\x1b[7;1H\x1b[?25h\x1b[?25l\x1b[9;1H\x1b[?25h";

    #[test]
    fn conpty_blank_lines_drawn_as_cursor_jumps_are_counted() {
        let ends = line_ends_reading_cursor_jumps(CONPTY_SNAPSHOT, true);
        assert_eq!(
            ends.len(),
            8,
            "four `\\r\\n` lines and four blank rows skipped by two jumps: {ends:?}"
        );
        // Counting `\r\n` alone is what the wait did, and it stops at 4 of the
        // 6 the test needed — the timeout, reproduced.
        assert_eq!(
            line_ends_reading_cursor_jumps(CONPTY_SNAPSHOT, false).len(),
            4
        );
        // Each skipped row ends where its jump does, so a probe's slice runs
        // from the previous line end through the jump: non-empty, no payload.
        let first_jump = CONPTY_SNAPSHOT
            .windows(b"\x1b[7;1H".len())
            .position(|window| window == b"\x1b[7;1H")
            .expect("the first jump")
            + b"\x1b[7;1H".len();
        assert_eq!(ends[4], first_jump);
        assert_eq!(ends[5], first_jump);
    }

    #[test]
    fn a_jump_up_or_to_the_current_row_advances_nothing() {
        // Row 1 → `\r\n` → row 2. `ESC[2;5H` stays on row 2 and `ESC[H` goes
        // home, so neither advances; the closing `ESC[3H` jumps from row 1 to
        // row 3, two advances ending where it does.
        let bytes = b"one\r\n\x1b[2;5Hx\x1b[Hy\x1b[3H";
        assert_eq!(
            line_ends_reading_cursor_jumps(bytes, true),
            vec![5, bytes.len(), bytes.len()]
        );
    }

    #[test]
    fn without_cursor_jumps_only_crlf_counts() {
        // The Unix arm is the old `\r\n` count, cursor sequences or not.
        let bytes = b"a\r\n\x1b[9;1Hb\r\n\r\n";
        assert_eq!(
            line_ends_reading_cursor_jumps(bytes, false),
            vec![3, 12, 14]
        );
        assert_eq!(
            line_ends_reading_cursor_jumps(bytes, false).len(),
            bytes.windows(2).filter(|window| *window == b"\r\n").count()
        );
    }
}
