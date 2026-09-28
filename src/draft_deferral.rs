//! Issue #544: an automatic FIRST write waits while the user has an unsent
//! draft in the target pane.
//!
//! Every daemon-originated first write — a delegate task pointer, a work-done
//! hand-off, a dispatch return, a scheduled or spawn-time seed, a reuse fire,
//! the deck's own status reports — is payload + CR. Written into an input box
//! that already holds a half-typed user message, that CR submits both as one
//! turn: `draft-textRead .dot-agent-deck/worker-task-coder.md for your task.`
//! Issue #424 closed the RETRY half of that (a repeat of bytes we already wrote
//! is refused once the user has typed since); this module is the first-write
//! half.
//!
//! The daemon cannot see an agent's input box, but it does see every byte a
//! deck client forwards into it (`PaneWriter`'s `Write` impl in
//! [`crate::agent_pty`]). So it keeps one bit per pane — [`DraftTracker`]: "the
//! user has sent input since their last submit or clear" — and a first write
//! consults it through [`decide_first_write`]: while the bit is set the write
//! WAITS, re-checking about every [`DRAFT_POLL_INTERVAL`], until the user
//! submits or clears, or until [`draft_defer_cap`] has passed. At the cap it
//! writes exactly as it did before this module existed and says so on the
//! pane's card. It never refuses and never drops, which is what lets it coexist
//! with #424's "a seed prompt must arrive" constraint.
//!
//! The bit is a PROXY, and it is wrong in both directions in ways that are all
//! bounded. Text an agent puts into its own box (history recall, autocomplete,
//! a restored message) is invisible to it, as is a partial clear it reads as a
//! full one, so those still concatenate as before. A key that edits nothing (a
//! menu answer with no Enter, a draft backspaced to empty) sets it, which costs
//! a delay of at most the cap and is cleared by the user's next Enter.
//!
//! Kept below both [`crate::agent_pty`] (which enforces the gate at its single
//! guarded writer) and [`crate::spawn`] (whose reuse fire shares its cap
//! budget), so neither has to depend on the other for it.

use std::time::{Duration, Instant};

/// Issue #544: overrides [`DEFAULT_DRAFT_DEFER_CAP`], in **milliseconds**. `0`
/// switches the gate off, restoring the pre-#544 immediate write exactly. A
/// non-numeric value falls back to the default with a `warn!`; a value above
/// [`MAX_DRAFT_DEFER_CAP`] is clamped to it.
///
/// Read once, when the daemon's [`crate::agent_pty::AgentPtyRegistry`] is
/// constructed, so it is set on the daemon's environment rather than the
/// client's.
pub const DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS: &str = "DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS";

/// Issue #544: how long an automatic first write waits for the user's draft
/// before writing anyway. Sixty seconds, the same bound as
/// [`crate::prompt_delivery::AUTOMATIC_PROMPT_DEADLINE`] and the scheduler's
/// reuse hard timeout: long enough to finish a thought, short enough that an
/// unattended orchestration stalled on a stray keystroke moves again on its
/// own.
pub const DEFAULT_DRAFT_DEFER_CAP: Duration = Duration::from_secs(60);

/// Issue #544: ceiling for [`DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS`]. Past ten
/// minutes an orchestration waiting on a draft nobody is finishing is stalled
/// rather than deferred.
pub const MAX_DRAFT_DEFER_CAP: Duration = Duration::from_secs(600);

/// Issue #544: how often a deferred first write re-reads the draft bit. It sets
/// how promptly delivery follows the user's Enter, and costs one mutex read per
/// waiting write per tick.
pub const DRAFT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The fixed card text published when a first write has waited out the whole
/// cap and is about to be written on top of the draft — published just before
/// the bytes go in, so the card already says so when the prompt appears. Must
/// contain the word `draft`: `scheduler/dispatch/023` keys on it.
pub const DRAFT_CAP_NOTICE: &str = "a deck prompt waited for the unsent draft in this pane until \
                                    the draft-deferral cap and was then submitted on top of it, so \
                                    your draft may have been sent together with it";

/// Resolve the cap from the environment. See
/// [`DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS`].
pub fn draft_defer_cap_from_env() -> Duration {
    parse_draft_defer_cap(
        std::env::var(DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS)
            .ok()
            .as_deref(),
    )
}

/// [`draft_defer_cap_from_env`] on an already-read value, so the parsing is
/// unit-testable without touching the process environment.
pub(crate) fn parse_draft_defer_cap(raw: Option<&str>) -> Duration {
    raw.and_then(|raw| {
        crate::state::parse_bounded_ms_override(
            DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS,
            raw,
            MAX_DRAFT_DEFER_CAP,
        )
    })
    .unwrap_or(DEFAULT_DRAFT_DEFER_CAP)
}

/// What a first write should do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstWriteDecision {
    /// Write. `capped` is `true` when the write goes ahead only because the cap
    /// has passed with the draft still pending — the degraded case that is
    /// reported on the pane's card.
    Now { capped: bool },
    /// Do not write yet; look again after this long.
    Wait { after: Duration },
}

/// Issue #544: the pure gate. `started` is when this delivery's wait budget
/// began (for a reuse fire, when its idle debounce began — the two share one
/// budget), and a zero `cap` means the gate is off.
pub fn decide_first_write(
    draft_pending: bool,
    now: Instant,
    started: Instant,
    cap: Duration,
) -> FirstWriteDecision {
    if !draft_pending || cap.is_zero() {
        return FirstWriteDecision::Now { capped: false };
    }
    let elapsed = now.saturating_duration_since(started);
    if elapsed >= cap {
        return FirstWriteDecision::Now { capped: true };
    }
    FirstWriteDecision::Wait {
        after: DRAFT_POLL_INTERVAL.min(cap - elapsed),
    }
}

/// Where [`DraftTracker`] is inside an escape sequence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Escape {
    #[default]
    Ground,
    /// An `ESC` whose meaning the next byte decides. `paste` when it arrived
    /// inside a bracketed paste, where it is content unless it opens the
    /// closing paste marker.
    Esc { paste: bool },
    /// `ESC [` — collecting a CSI sequence.
    Csi {
        /// A private marker (`<`, `=`, `>`, `?`) as the first parameter byte.
        /// Only terminal REPORTS carry one; no key the deck forwards does.
        private: bool,
        params: bool,
        dollar: bool,
        len: u8,
        /// The parameters read as one decimal number while they are nothing
        /// but digits, `None` once anything else appears — enough to tell the
        /// paste markers `ESC[200~` / `ESC[201~` from every other `~` key.
        code: Option<u16>,
        /// Started inside a bracketed paste: every sequence but the closing
        /// marker is content there.
        paste: bool,
    },
    /// `ESC O` — an SS3 key (F1–F4, application-mode arrows).
    Ss3,
    /// `ESC ]` — an OSC reply, ended by `BEL` or `ESC \`.
    Osc { len: u16, esc: bool },
    /// `ESC P` — a DCS reply, ended by `ESC \`.
    Dcs { len: u16, esc: bool },
    /// The three raw bytes after a legacy X10 mouse report's `ESC [ M`.
    X10 { remaining: u8 },
}

/// A CSI sequence longer than this is not a report the deck knows, so it is
/// treated as input rather than held in the parser.
const MAX_CSI_LEN: u8 = 32;

/// An OSC/DCS reply longer than this is treated as input: a user who typed
/// `Alt+]` and kept typing must not have the rest of their draft swallowed for
/// good.
const MAX_STRING_LEN: u16 = 4096;

/// Issue #544: "has the user sent input into this pane since their last submit
/// or clear?", decided from the bytes a deck client forwards.
///
/// **Sets it:** every byte that is not part of a recognised terminal REPORT —
/// printable text and UTF-8, arrows and other navigation keys (editing a
/// history-recalled prompt is editing a draft), `Backspace`, `Tab`, `LF`
/// (`Ctrl+J`), `ESC CR` (`Alt+Enter`), `Shift+Enter`'s `ESC[13;2u`, and
/// anything inside a bracketed paste.
///
/// **Clears it:** a byte that SUBMITS the input box, as decided by the caller
/// (`crate::agent_pty`'s paste framing plus
/// [`crate::ui::user_byte_submits_input_box`]), and outside a paste `Ctrl+U`
/// (`0x15`, line-kill) and `Ctrl+C` (`0x03`). Both clear keys are
/// approximations — `Ctrl+U` kills only the current line of a multi-line
/// draft in Claude Code — and an approximation here can only let a draft
/// through as before, never hold a prompt past the cap.
///
/// **Neither:** terminal reports, which a client forwards through the same
/// channel as keystrokes (xterm.js emits its query replies through `onData`):
/// SGR and X10 mouse (`ESC[<…M`/`m`, `ESC[M` + 3 bytes), focus (`ESC[I`,
/// `ESC[O`), CPR (`ESC[…R`), DA (`ESC[…c`), DSR (`ESC[…n`), DECRPM
/// (`ESC[…$y`), window reports (`ESC[…t`), any CSI with a private marker, and
/// OSC and DCS strings. Nor do the bracketed-paste markers themselves
/// (`ESC[200~`, `ESC[201~`): an empty paste sends nothing into the box, so
/// only the bytes between them count. One ambiguity is accepted: xterm encodes a modified `F3`
/// as `ESC[1;5R`, the shape of a CPR, so that key does not set the bit. The
/// deck's own encoder sends `F3` as `ESC O R` and is unaffected.
///
/// State is carried across calls, so a report split between two writes is
/// still recognised.
#[derive(Debug, Default)]
pub(crate) struct DraftTracker {
    pending: bool,
    escape: Escape,
}

impl DraftTracker {
    pub(crate) fn pending(&self) -> bool {
        self.pending
    }

    /// Our own `Applied` SUBMIT write sent a CR, which submitted whatever was
    /// in the box — the user's draft included.
    pub(crate) fn clear(&mut self) {
        self.pending = false;
    }

    /// Feed one user byte. `submits` is whether it submitted the input box and
    /// `in_paste` whether it arrived inside a bracketed paste — both decided by
    /// the caller's stream state, which owns paste framing.
    pub(crate) fn feed_byte(&mut self, byte: u8, submits: bool, in_paste: bool) {
        if submits {
            // Enter is a hard reset: whatever sequence was being parsed is over.
            self.pending = false;
            self.escape = Escape::Ground;
            return;
        }
        self.escape = self.step(byte, in_paste);
    }

    fn step(&mut self, byte: u8, in_paste: bool) -> Escape {
        const ESC: u8 = 0x1b;
        const BEL: u8 = 0x07;
        match self.escape {
            Escape::Ground => self.ground(byte, in_paste),
            Escape::Esc { paste: true } => match byte {
                b'[' => Escape::Csi {
                    private: false,
                    params: false,
                    dollar: false,
                    len: 0,
                    code: Some(0),
                    paste: true,
                },
                // Pasted content after all: count the ESC and read this byte
                // afresh, still inside the paste.
                _ => {
                    self.pending = true;
                    self.ground(byte, in_paste)
                }
            },
            Escape::Esc { paste: false } => match byte {
                b'[' => Escape::Csi {
                    private: false,
                    params: false,
                    dollar: false,
                    len: 0,
                    code: Some(0),
                    paste: false,
                },
                b']' => Escape::Osc { len: 0, esc: false },
                b'P' => Escape::Dcs { len: 0, esc: false },
                b'O' => Escape::Ss3,
                // The first ESC was a key of its own; this one starts afresh.
                ESC => {
                    self.pending = true;
                    Escape::Esc { paste: false }
                }
                // `Alt+<key>`, `Alt+Enter`, `Alt+Backspace`: an edit.
                _ => {
                    self.pending = true;
                    Escape::Ground
                }
            },
            Escape::Csi {
                private,
                params,
                dollar,
                len,
                code,
                paste,
            } => match byte {
                0x30..=0x3f => {
                    if len >= MAX_CSI_LEN {
                        self.pending = true;
                        return Escape::Ground;
                    }
                    let is_private = !params && matches!(byte, b'<' | b'=' | b'>' | b'?');
                    let code = code.and_then(|code| match byte {
                        b'0'..=b'9' => code
                            .checked_mul(10)
                            .and_then(|code| code.checked_add(u16::from(byte - b'0'))),
                        _ => None,
                    });
                    Escape::Csi {
                        private: private || is_private,
                        params: true,
                        dollar,
                        len: len + 1,
                        code,
                        paste,
                    }
                }
                0x20..=0x2f => Escape::Csi {
                    private,
                    params,
                    dollar: dollar || byte == b'$',
                    len: len.saturating_add(1),
                    code: None,
                    paste,
                },
                0x40..=0x7e => {
                    // Exactly the bytes `crate::agent_pty`'s paste framing
                    // matches, so the two agree on what a marker is.
                    let paste_marker =
                        byte == b'~' && len == 3 && matches!(code, Some(200 | 201));
                    if paste_marker {
                        return Escape::Ground;
                    }
                    if paste {
                        // Inside a paste, only the closing marker is framing.
                        self.pending = true;
                        return Escape::Ground;
                    }
                    if !params && byte == b'M' {
                        return Escape::X10 { remaining: 3 };
                    }
                    let report = private
                        || matches!(byte, b'M' | b'm' | b'R' | b'c' | b'n' | b't')
                        || (dollar && byte == b'y')
                        || (!params && matches!(byte, b'I' | b'O'));
                    if !report {
                        self.pending = true;
                    }
                    Escape::Ground
                }
                // Not a CSI after all: count what was sent as input and read
                // this byte afresh.
                _ => {
                    self.pending = true;
                    self.ground(byte, in_paste)
                }
            },
            Escape::Ss3 => {
                self.pending = true;
                Escape::Ground
            }
            Escape::Osc { len, esc } | Escape::Dcs { len, esc } => {
                let is_osc = matches!(self.escape, Escape::Osc { .. });
                let next = |len: u16, esc: bool| {
                    if is_osc {
                        Escape::Osc { len, esc }
                    } else {
                        Escape::Dcs { len, esc }
                    }
                };
                if esc {
                    return if byte == b'\\' {
                        Escape::Ground
                    } else {
                        // An unterminated string followed by a new sequence.
                        self.escape = Escape::Esc { paste: false };
                        self.step(byte, in_paste)
                    };
                }
                match byte {
                    ESC => next(len, true),
                    BEL if is_osc => Escape::Ground,
                    // A control byte inside a reply means it was never one.
                    0x00..=0x1f => {
                        self.pending = true;
                        self.ground(byte, in_paste)
                    }
                    _ if len >= MAX_STRING_LEN => {
                        self.pending = true;
                        Escape::Ground
                    }
                    _ => next(len + 1, false),
                }
            }
            Escape::X10 { remaining } => {
                if remaining > 1 {
                    Escape::X10 {
                        remaining: remaining - 1,
                    }
                } else {
                    Escape::Ground
                }
            }
        }
    }

    fn ground(&mut self, byte: u8, in_paste: bool) -> Escape {
        match byte {
            // Inside a paste an ESC is content — unless it opens the closing
            // marker, which `Escape::Esc { paste: true }` decides.
            0x1b => Escape::Esc { paste: in_paste },
            0x15 | 0x03 if !in_paste => {
                self.pending = false;
                Escape::Ground
            }
            _ => {
                self.pending = true;
                Escape::Ground
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tracker: &mut DraftTracker, bytes: &[u8]) {
        let mut preceding = None;
        for &byte in bytes {
            let submits = crate::ui::user_byte_submits_input_box(preceding, byte);
            tracker.feed_byte(byte, submits, false);
            preceding = Some(byte);
        }
    }

    fn pending_after(bytes: &[u8]) -> bool {
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, bytes);
        tracker.pending()
    }

    #[test]
    fn decide_first_write_writes_when_no_draft_is_pending() {
        let now = Instant::now();
        assert_eq!(
            decide_first_write(false, now, now, DEFAULT_DRAFT_DEFER_CAP),
            FirstWriteDecision::Now { capped: false }
        );
    }

    #[test]
    fn decide_first_write_waits_one_poll_while_a_draft_is_pending() {
        let started = Instant::now();
        assert_eq!(
            decide_first_write(
                true,
                started + Duration::from_secs(1),
                started,
                DEFAULT_DRAFT_DEFER_CAP
            ),
            FirstWriteDecision::Wait {
                after: DRAFT_POLL_INTERVAL
            }
        );
    }

    #[test]
    fn decide_first_write_never_sleeps_past_the_cap() {
        let started = Instant::now();
        let cap = Duration::from_millis(1000);
        assert_eq!(
            decide_first_write(true, started + Duration::from_millis(950), started, cap),
            FirstWriteDecision::Wait {
                after: Duration::from_millis(50)
            }
        );
    }

    #[test]
    fn decide_first_write_writes_capped_at_and_after_the_cap() {
        let started = Instant::now();
        let cap = Duration::from_millis(1000);
        for elapsed in [1000, 5000] {
            assert_eq!(
                decide_first_write(true, started + Duration::from_millis(elapsed), started, cap),
                FirstWriteDecision::Now { capped: true },
                "{elapsed} ms"
            );
        }
    }

    #[test]
    fn decide_first_write_zero_cap_is_the_gate_off() {
        let now = Instant::now();
        assert_eq!(
            decide_first_write(true, now, now, Duration::ZERO),
            FirstWriteDecision::Now { capped: false }
        );
    }

    #[test]
    fn decide_first_write_tolerates_a_start_in_the_future() {
        let now = Instant::now();
        assert_eq!(
            decide_first_write(
                true,
                now,
                now + Duration::from_secs(5),
                Duration::from_secs(1)
            ),
            FirstWriteDecision::Wait {
                after: DRAFT_POLL_INTERVAL
            }
        );
    }

    #[test]
    fn draft_defer_cap_parses_default_zero_value_and_ceiling() {
        assert_eq!(parse_draft_defer_cap(None), DEFAULT_DRAFT_DEFER_CAP);
        assert_eq!(parse_draft_defer_cap(Some("0")), Duration::ZERO);
        assert_eq!(
            parse_draft_defer_cap(Some("1500")),
            Duration::from_millis(1500)
        );
        assert_eq!(parse_draft_defer_cap(Some("99999999")), MAX_DRAFT_DEFER_CAP);
        assert_eq!(parse_draft_defer_cap(Some("soon")), DEFAULT_DRAFT_DEFER_CAP);
    }

    #[test]
    fn draft_bit_is_set_by_printable_utf8_and_navigation_keys() {
        assert!(pending_after(b"hello"));
        assert!(pending_after("żółw".as_bytes()));
        for key in [
            &b"\x1b[A"[..],
            b"\x1b[B",
            b"\x1b[C",
            b"\x1b[D",
            b"\x1b[1;2A",
            b"\x1b[H",
            b"\x1b[3~",
            b"\x1b[Z",
            b"\x1bOP",
            b"\x7f",
            b"\t",
        ] {
            assert!(pending_after(key), "{key:?}");
        }
    }

    #[test]
    fn draft_bit_is_cleared_by_enter_ctrl_u_and_ctrl_c() {
        for clear in [&b"\r"[..], b"\x15", b"\x03"] {
            let mut bytes = b"draft".to_vec();
            bytes.extend_from_slice(clear);
            assert!(!pending_after(&bytes), "{clear:?}");
        }
    }

    #[test]
    fn draft_bit_survives_newline_keys() {
        // Ctrl+J, Alt+Enter and Shift+Enter keep the user typing.
        for newline in [&b"\n"[..], b"\x1b\r", b"\x1b[13;2u"] {
            let mut bytes = b"draft".to_vec();
            bytes.extend_from_slice(newline);
            assert!(pending_after(&bytes), "{newline:?}");
            // ...and set the bit on an empty box too.
            assert!(pending_after(newline), "{newline:?} alone");
        }
    }

    #[test]
    fn terminal_reports_neither_set_nor_clear_the_draft_bit() {
        let reports: [&[u8]; 11] = [
            b"\x1b[<64;10;5M",
            b"\x1b[<0;3;4m",
            b"\x1b[M !!",
            b"\x1b[I",
            b"\x1b[O",
            b"\x1b[10;5R",
            b"\x1b[?1;2c",
            b"\x1b[0n",
            b"\x1b[?2004;1$y",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
            b"\x1bP1$r0m\x1b\\",
        ];
        for report in reports {
            assert!(!pending_after(report), "report set the bit: {report:?}");
            let mut drafted = b"draft".to_vec();
            drafted.extend_from_slice(report);
            assert!(
                pending_after(&drafted),
                "report cleared the bit: {report:?}"
            );
        }
        assert!(!pending_after(b"\x1b]11;rgb:0000/0000/0000\x1b\\"));
    }

    #[test]
    fn a_report_split_across_writes_is_still_a_report() {
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"\x1b[<64;1");
        feed(&mut tracker, b"0;5M");
        feed(&mut tracker, b"\x1b");
        feed(&mut tracker, b"]10;rgb:ff");
        feed(&mut tracker, b"ff/ffff/ffff\x07");
        assert!(!tracker.pending());
        feed(&mut tracker, b"x");
        assert!(tracker.pending());
    }

    #[test]
    fn text_after_a_report_still_sets_the_bit() {
        assert!(!pending_after(b"\x1b[I"));
        assert!(pending_after(b"\x1b[Ihello"));
        assert!(pending_after(b"\x1b[<64;10;5Mx"));
    }

    #[test]
    fn a_long_unterminated_osc_does_not_swallow_a_draft_for_good() {
        let mut bytes = b"\x1b]".to_vec();
        bytes.extend(std::iter::repeat_n(b'a', usize::from(MAX_STRING_LEN) + 1));
        assert!(pending_after(&bytes));
        // A control byte ends a would-be reply too.
        assert!(pending_after(b"\x1b]half\t"));
        assert!(!pending_after(b"\x1b]half\x15"));
    }

    #[test]
    fn inside_a_paste_every_byte_is_content() {
        let mut tracker = DraftTracker::default();
        for &byte in b"\x15\x03\x1b" {
            tracker.feed_byte(byte, false, true);
            assert!(tracker.pending(), "{byte:#x}");
        }
    }

    /// Feed `bytes` wrapped in bracketed-paste markers, with the paste framing
    /// `crate::agent_pty`'s stream supplies: the state BEFORE each byte, so the
    /// opening marker is outside the paste and the closing one inside it.
    fn feed_paste(tracker: &mut DraftTracker, bytes: &[u8]) {
        for &byte in b"\x1b[200~" {
            tracker.feed_byte(byte, false, false);
        }
        for &byte in bytes {
            tracker.feed_byte(byte, false, true);
        }
        for &byte in b"\x1b[201~" {
            tracker.feed_byte(byte, false, true);
        }
    }

    #[test]
    fn an_empty_paste_does_not_set_the_bit() {
        let mut tracker = DraftTracker::default();
        feed_paste(&mut tracker, b"");
        assert!(!tracker.pending(), "the paste markers alone set the bit");
        // Split across writes, as a client may send them.
        let mut tracker = DraftTracker::default();
        let chunks: [(&[u8], bool); 5] = [
            (b"\x1b[2", false),
            (b"00~", false),
            (b"\x1b", true),
            (b"[201", true),
            (b"~", true),
        ];
        for (chunk, in_paste) in chunks {
            for &byte in chunk {
                tracker.feed_byte(byte, false, in_paste);
            }
        }
        assert!(!tracker.pending(), "a split empty paste set the bit");
    }

    #[test]
    fn paste_markers_do_not_clear_a_draft() {
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"draft");
        feed_paste(&mut tracker, b"");
        assert!(tracker.pending());
    }

    #[test]
    fn pasted_content_still_sets_the_bit() {
        let contents: [&[u8]; 6] = [
            b"x",
            b"\x1b",
            b"\x1b\x1b",
            b"\x1b[I",
            b"\x1b[<64;10;5M",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
        ];
        for content in contents {
            let mut tracker = DraftTracker::default();
            feed_paste(&mut tracker, content);
            assert!(tracker.pending(), "pasted {content:?} did not set the bit");
        }
        // An end marker right after pasted text leaves the bit set, and text
        // typed after an empty paste sets it.
        let mut tracker = DraftTracker::default();
        feed_paste(&mut tracker, b"");
        feed(&mut tracker, b"y");
        assert!(tracker.pending());
    }

    #[test]
    fn only_the_exact_marker_bytes_are_framing() {
        // Other `~` keys, and near-misses of the marker, are still keys.
        for key in [
            &b"\x1b[2~"[..],
            b"\x1b[20~",
            b"\x1b[202~",
            b"\x1b[0200~",
            b"\x1b[2000~",
            b"\x1b[200;1~",
            b"\x1b[200$~",
        ] {
            assert!(pending_after(key), "{key:?} did not set the bit");
        }
        assert!(!pending_after(b"\x1b[200~"));
        assert!(!pending_after(b"\x1b[201~"));
    }

    #[test]
    fn our_own_submit_clears_the_bit() {
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"draft");
        tracker.clear();
        assert!(!tracker.pending());
    }
}
