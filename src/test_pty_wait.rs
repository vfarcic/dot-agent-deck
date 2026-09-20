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

/// Line terminators the pane's byte target has produced so far — a caller's
/// clock, and it is made of CONTENT rather than of time.
///
/// Every completed input line puts exactly TWO of them into the buffer,
/// whatever was written. The line discipline echoes the payload and the
/// delayed submit CR back as one `<payload>\r\n`, and `/bin/cat` copies the
/// same line straight back as a second `<payload>\r\n`; a submit-only probe
/// writes no payload, so its two lines are simply `\r\n\r\n`. Nothing else
/// can reach this PTY — the target is a bare `/bin/cat` with no wrapper in
/// front of it, which is what every caller's spawn helper guarantees — so `2N`
/// terminators means exactly "lines 1..=N have landed IN FULL", and the last
/// byte any of them produces is the `\n` that makes its second one.
///
/// Callers in `spawn.rs` count DELIVERY ATTEMPTS with it, one line each; the
/// `ui.rs` and `state.rs` callers count writes. The unit is the same.
pub(crate) fn completed_lines(bytes: &[u8]) -> usize {
    bytes.windows(2).filter(|window| *window == b"\r\n").count()
}

/// Block until `lines` completed input lines have finished round-tripping,
/// i.e. until the buffer holds the `2 * lines` line terminators
/// [`completed_lines`] documents — one per line from the echo, one from
/// `/bin/cat`'s copy.
///
/// This is the same wait as `spawn.rs`'s `wait_for_detached_delivery_attempt`
/// said in the vocabulary of a caller that is counting WRITES rather than
/// delivery attempts. A caller needs it before snapshotting a baseline that a later
/// assertion compares by EXACT EQUALITY: a `cat` copy still in flight lands
/// inside the comparison window and reads as bytes the code under test
/// sent, which is issue #850's failure mode one assertion further on from
/// the precondition it was filed for.
pub(crate) async fn wait_for_drained_lines(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    lines: usize,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let snapshot = registry.snapshot(agent_id).expect("byte target snapshot");
        let seen = completed_lines(&snapshot);
        if seen >= 2 * lines {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {lines} line(s) to finish round-tripping; saw \
             {seen} of {} terminators; snapshot={:?}",
            2 * lines,
            String::from_utf8_lossy(&snapshot)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The printable runs of `bytes` that are long enough to search the
/// target's buffer for.
///
/// The line discipline echoes printable input verbatim, so every such run
/// must appear in the buffer once the write has physically reached the PTY.
/// Control bytes are excluded because their echoed form is not fixed —
/// `ECHOCTL` renders `ESC` as `^[` while `/bin/cat`'s copy of the same line
/// carries the raw byte — which is also why the runs are searched for
/// individually instead of the payload being searched for whole. One-byte
/// runs are dropped: a single character is not a landmark.
pub(crate) fn printable_runs(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split(|byte: &u8| !byte.is_ascii_graphic() && *byte != b' ')
        .filter(|run| run.len() >= 2)
        .collect()
}

/// `bytes` with terminal escape sequences removed, leaving the text a
/// terminal is expected to echo.
///
/// A landmark has to be TEXT, not protocol, and getting that wrong is what
/// made the first version of this fix pass `build` and fail `build-windows`
/// on the same commit. `ESC[200~` is a real bracketed-paste marker: Unix's
/// line discipline knows nothing about it and echoes its printable tail
/// (`[200~`) literally, while Windows' ConPTY PARSES it and echoes nothing
/// at all. A landmark cut from the raw payload therefore matched on one
/// platform and could never match on the other — and the callers' own
/// preconditions never looked for it either, only for the draft text.
pub(crate) fn echo_text(bytes: &[u8]) -> Vec<u8> {
    let mut text = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((first, tail)) = rest.split_first() {
        if *first != 0x1b {
            text.push(*first);
            rest = tail;
            continue;
        }
        // CSI: `ESC [`, parameter and intermediate bytes, then a final byte
        // in 0x40..=0x7e. Anything else after ESC is a two-byte sequence.
        rest = match tail.split_first() {
            Some((b'[', params)) => params
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
                .map_or(&[][..], |end| &params[end + 1..]),
            Some((_, after)) => after,
            None => &[][..],
        };
    }
    text
}

/// The text landmarks a write must leave on the target before it is known
/// to have physically reached the PTY.
///
/// Each is waited for at ONE appearance — the echo. Whether the byte target
/// also copies the text back depends on the platform, so that half is not
/// predicted here; [`wait_for_quiescent_lines`] observes it instead.
pub(crate) fn echo_landmarks(bytes: &[u8]) -> Vec<Vec<u8>> {
    printable_runs(&echo_text(bytes))
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect()
}

/// Line terminators in `bytes`, each of which MAY complete an input line —
/// whether it does is the platform's business, not this fixture's.
pub(crate) fn line_terminators(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .filter(|byte| matches!(**byte, b'\n' | b'\r'))
        .count()
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

/// Block until the pane is quiescent again after a write: every line
/// terminator the platform actually honoured has produced BOTH its echo and
/// the byte target's copy of the finished line.
///
/// Deliberately OBSERVED rather than predicted, which is what makes it
/// portable. [`completed_lines`] counts two terminators per completed line,
/// so a pane with nothing in flight always holds an EVEN count; a caller
/// that drained the pane before writing therefore only has to wait for the
/// count to become even again. That absorbs the platform difference instead
/// of encoding it: Unix honours the `\n` inside a bracketed paste and
/// produces two more terminators, Windows' ConPTY consumes the paste markers
/// and produces none, and both are simply "even".
///
/// Two preconditions, both enforced by the callers rather than assumed here.
/// The pane must have been drained first, or an earlier copy still in flight
/// could make the count even at the wrong moment. And the write must carry
/// at most ONE terminator — two would take the count from even straight to
/// even and let this return before either copy landed — which is why
/// [`type_user_bytes`] and `spawn.rs`'s `type_user_frame` both assert it.
pub(crate) async fn wait_for_quiescent_lines(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    drained: usize,
) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let snapshot = registry.snapshot(agent_id).expect("byte target snapshot");
        let seen = completed_lines(&snapshot);
        if seen >= drained && seen.is_multiple_of(2) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the pane to go quiescent: saw {seen} terminators, \
             wanted an even count of at least {drained}; snapshot={:?}",
            String::from_utf8_lossy(&snapshot)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Put raw user-input bytes on the pane's PTY and record them as user
/// input. Waits for nothing — the callers below own that half.
pub(crate) async fn write_user_bytes(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    pane_id: &str,
    bytes: &[u8],
) {
    use std::io::Write as _;

    let handle = registry
        .subscribe(agent_id)
        .expect("attach detached byte-observation target");
    let mut writer = handle.writer.lock().await;
    writer
        .write_all(bytes)
        .expect("write detached user input bytes");
    writer.flush().expect("flush detached user input bytes");
    drop(writer);
    registry.note_user_input(pane_id);
}

/// Type user-input bytes, returning only once they are demonstrably on the
/// target's PTY — and only once nothing else is still arriving on it.
///
/// This used to write and then sleep a fixed 75 ms (issue #850). Nothing
/// waited for the bytes to become observable, so every caller's "the draft
/// must physically reach the PTY" precondition was a bet on a PTY round trip
/// beating that constant — a bet a contended runner loses, and the fast tier
/// has no retries, so losing it hard-reds the required `build` job. Both
/// waits below are keyed on content and bounded only by a diagnostic
/// deadline, so load makes them slower rather than wrong.
///
/// `lines_already_written` is the number of input lines the pane has
/// completed before this call — one per guarded delivery, which writes a
/// payload and a submit CR. Draining them first is not tidiness, and it is
/// the half of this fix that is easiest to mistake for it: `/bin/cat` and
/// the line discipline's echo are two independent writers into one PTY
/// output queue, and the kernel commits echo in batches, so a `cat` copy
/// that is still pending can land IN THE MIDDLE of this write's echo.
/// Measured, on the plain-Enter case of
/// `dispatch_020_payload_guards_are_scoped_to_one_delivery`:
///
/// ```text
/// "automatic payload before plain Enter\r\n"   <- echo of the earlier line
/// "user"                                       <- this write's echo, cut off
/// "automatic payload before plain Enter\r\n"   <- cat's copy, interleaved
/// " turn completed with plain Enter"           <- the rest of this write
/// ```
///
/// The scrollback is append-only, so that split is permanent: no amount of
/// waiting reassembles the draft, and every `windows(draft.len())` search in
/// the callers below — their own preconditions included — fails for good.
/// With the pane drained first, `cat` has nothing to copy until this write
/// terminates a line, so there is no second writer to interleave.
pub(crate) async fn type_user_bytes(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    pane_id: &str,
    bytes: &[u8],
    lines_already_written: usize,
) {
    let landmarks = echo_landmarks(bytes);
    assert!(
        !landmarks.is_empty(),
        "cannot observe this write's own completion: {:?} carries no text \
         landmark. Use type_user_frame, which is written for a payload that \
         is nothing but a control frame.",
        String::from_utf8_lossy(bytes)
    );
    assert!(
        line_terminators(bytes) <= 1,
        "wait_for_quiescent_lines cannot bracket a write carrying more than one \
         line terminator: {:?}",
        String::from_utf8_lossy(bytes)
    );
    let drained =
        completed_lines(&wait_for_drained_lines(registry, agent_id, lines_already_written).await);
    write_user_bytes(registry, agent_id, pane_id, bytes).await;
    for needle in &landmarks {
        wait_for_echo_bytes(registry, agent_id, needle).await;
    }
    wait_for_quiescent_lines(registry, agent_id, drained).await;
}

pub(crate) async fn type_user_draft(
    registry: &AgentPtyRegistry,
    agent_id: &str,
    pane_id: &str,
    draft: &str,
    lines_already_written: usize,
) {
    type_user_bytes(
        registry,
        agent_id,
        pane_id,
        draft.as_bytes(),
        lines_already_written,
    )
    .await;
}
