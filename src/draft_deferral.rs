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
//! The daemon cannot see an agent's input box, but it does see every byte that
//! reaches it: what a deck client forwards (`PaneWriter`'s `Write` impl in
//! [`crate::agent_pty`]) and what the daemon writes itself. So it keeps one bit
//! per pane — [`DraftTracker`]: "the
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

/// The fixed `DeliveryNotice` detail published when a first write has waited
/// out the whole cap and is about to be written on top of the draft — published
/// just before the bytes go in. The daemon's sink carries it on a synthetic
/// `Error` event, as `tool_detail` and under `DELIVERY_NOTICE_METADATA_KEY`,
/// but no client renders that text today: the TUI's session card shows only
/// the `Error` badge, and the desktop app shows the `error` status and a
/// generic error entry. The readable record of why is the `warn!` logged beside
/// the publish, present only when the daemon runs with `DOT_AGENT_DECK_LOG`.
/// Must contain the word `draft`: `scheduler/dispatch/023` keys on it.
pub const DRAFT_CAP_NOTICE: &str = "a deck prompt waited for the unsent draft in this pane until \
                                    the draft-deferral cap and was then submitted on top of it, so \
                                    your draft may have been sent together with it";

/// Issue #544 (PR #1398 review): the fixed `DeliveryNotice` detail published
/// when a delegated task pointer waited for the unsent draft in a worker pane
/// and, by the time it could be written, the worker it was meant for had been
/// replaced — most often by `pane restart`, which does not wait for a delegate's
/// draft wait to end. The pointer is not written to the replacement. Reported on
/// the pane's current occupant when a live one exists by then, the only one
/// whose card the sink will mark; like [`DRAFT_CAP_NOTICE`], what a user sees of
/// it is the card turning `Error`. When the refusal lands while the replacement
/// is not yet live, nothing is published and only the daemon log records it.
pub const DRAFT_WAIT_WORKER_REPLACED_NOTICE: &str = "a delegated task pointer waited for the \
                                                     unsent draft in this pane, and the worker \
                                                     it was meant for was replaced before it \
                                                     could be written, so it was not delivered";

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
    ///
    /// `esc` is `Some` once an `ESC` has arrived inside the string: the next
    /// byte decides whether it begins the `ESC \` terminator or a new
    /// sequence. It holds whether the USER sent any of the string before that
    /// `ESC` — the credit the string is owed if it turns out never to have
    /// been a reply.
    Osc { len: u16, esc: Option<bool> },
    /// `ESC P` — a DCS reply, ended by `ESC \`. `esc` as for [`Self::Osc`].
    Dcs { len: u16, esc: Option<bool> },
    /// The three raw bytes after a legacy X10 mouse report's `ESC [ M`. Each
    /// is 32 plus a value, so a control byte ends it as never having been one.
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
/// or clear?", decided from the pane's input stream.
///
/// **Sets it:** every USER byte that is not part of a recognised terminal REPORT —
/// printable text and UTF-8, arrows and other navigation keys (editing a
/// history-recalled prompt is editing a draft), `Backspace`, `Tab`, `LF`
/// (`Ctrl+J`), `ESC CR` (`Alt+Enter`), `Shift+Enter`'s `ESC[13;2u`, and
/// anything inside a bracketed paste.
///
/// **Clears it:** a byte that SUBMITS the input box, whoever sent it, as
/// decided by the caller (`crate::agent_pty`'s paste framing plus
/// [`crate::ui::user_byte_submits_input_box`]), and outside a paste the user's
/// `Ctrl+U` (`0x15`, line-kill) and `Ctrl+C` (`0x03`). Both clear keys are
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
/// only the bytes between them count. Inside a paste only `ESC[201~` is a
/// marker; an `ESC[200~` there is pasted text and sets the bit. One ambiguity is accepted: xterm encodes a modified `F3`
/// as `ESC[1;5R`, the shape of a CPR, so that key does not set the bit. The
/// deck's own encoder sends `F3` as `ESC O R` and is unaffected.
///
/// State is carried across calls, so a report split between two writes is
/// still recognised.
///
/// **One stream, two senders** (PR #1398 finding #16). The pane's PTY reads
/// ONE byte stream: the user's keystrokes and the deck's own writes, in the
/// order they were written. The parser here, like `crate::agent_pty`'s paste
/// framing, is fed all of it, because a deck write moves the agent's parser
/// exactly as a keystroke does — its CR ends a half-sent sequence, its own
/// paste markers open and close a paste. [`ByteOrigin`] decides only what a
/// byte does to the BIT: a user byte of content sets it, a deck byte never
/// does, and a byte that submits clears it whoever sent it. The clear keys
/// (`Ctrl+U`, `Ctrl+C`) clear it only from the user; the deck never sends
/// them, and if it did, keeping the bit is the bounded direction.
///
/// A sequence is input on behalf of whoever sent its bytes after the `ESC`:
/// a lone `ESC` followed by a deck byte is the user's Escape key and the
/// deck's text, so it sets nothing whatever that text starts with, while a
/// user's `Alt+]` and the text they kept typing still count when a deck byte
/// is what ends the would-be reply. A sequence that was never one — an
/// unterminated string, an overlong CSI, an X10 or SS3 prefix cut short by a
/// control byte — is input for whoever sent it, and the byte that ended it is
/// read afresh for its own sender (PR #1398 finding #17).
#[derive(Debug, Default)]
pub(crate) struct DraftTracker {
    pending: bool,
    escape: Escape,
    /// Whether a USER byte is part of the sequence being parsed, counting the
    /// bytes AFTER the `ESC` that opened it — and that `ESC` too inside a
    /// paste, where it is content. Meaningless in [`Escape::Ground`].
    seq_user: bool,
    /// Whether the user sent the `ESC` that opened the sequence being parsed.
    /// Outside a paste it counts by itself only as the lone Escape key of
    /// `ESC ESC`, and only when the user sent the second one too.
    esc_user: bool,
    /// Whether the byte being fed is the user's. Set for each byte by
    /// [`Self::feed_byte`].
    by_user: bool,
    /// Set by [`Self::ground`] when this byte opened a NEW sequence, whose
    /// sender is this byte's alone.
    fresh: bool,
}

/// Who wrote a byte into a pane's PTY input. See [`DraftTracker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ByteOrigin {
    /// Forwarded from an attached deck client: the user typing.
    User,
    /// Written by the daemon itself: a payload, its paste framing, its CR or
    /// LF, or the erases that take a partial write back out.
    Deck,
}

impl DraftTracker {
    pub(crate) fn pending(&self) -> bool {
        self.pending
    }

    /// Feed one byte of the pane's input stream. `submits` is whether it
    /// submitted the input box and `in_paste` whether it arrived inside a
    /// bracketed paste — both decided by the caller's stream state, which owns
    /// paste framing — and `origin` who wrote it.
    pub(crate) fn feed_byte(
        &mut self,
        byte: u8,
        submits: bool,
        in_paste: bool,
        origin: ByteOrigin,
    ) {
        if submits {
            // Enter is a hard reset, whoever pressed it: the box was
            // submitted, and whatever sequence was being parsed is over.
            self.pending = false;
            self.escape = Escape::Ground;
            return;
        }
        self.by_user = origin == ByteOrigin::User;
        self.fresh = false;
        let next = self.step(byte, in_paste);
        match next {
            Escape::Ground => self.seq_user = false,
            _ if self.fresh => {}
            _ => self.seq_user |= self.by_user,
        }
        self.escape = next;
    }

    /// The byte being fed is input: set the bit if the user sent it.
    fn input_byte(&mut self) {
        self.pending |= self.by_user;
    }

    /// The sequence parsed so far, WITHOUT the byte being fed, is input.
    fn input_sequence(&mut self) {
        self.pending |= self.seq_user;
    }

    /// The sequence up to and including the byte being fed is input.
    fn input_through(&mut self) {
        self.pending |= self.seq_user || self.by_user;
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
                    self.input_sequence();
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
                b']' => Escape::Osc { len: 0, esc: None },
                b'P' => Escape::Dcs { len: 0, esc: None },
                b'O' => Escape::Ss3,
                // The first ESC was a key of its own; this one starts afresh,
                // attributed to its own sender by `ground`. The first is
                // counted only when the user sent it (`esc_user`, not this
                // byte's origin) AND is the one who went on typing: a deck
                // ESC is never theirs, and a lone Escape keypress followed by
                // a deck write edits nothing of theirs.
                ESC => {
                    self.pending |= self.esc_user && self.by_user;
                    self.ground(byte, in_paste)
                }
                // `Alt+<key>`, `Alt+Enter`, `Alt+Backspace`: an edit, and the
                // key's sender is the one who made it.
                _ => {
                    self.input_byte();
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
                        self.input_through();
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
                0x20..=0x2f => {
                    // Bounded like the parameter bytes above: past the bound
                    // this is input, not a report the deck knows.
                    if len >= MAX_CSI_LEN {
                        self.input_through();
                        return Escape::Ground;
                    }
                    Escape::Csi {
                        private,
                        params,
                        dollar: dollar || byte == b'$',
                        len: len + 1,
                        code: None,
                        paste,
                    }
                }
                0x40..=0x7e => {
                    // Exactly the bytes `crate::agent_pty`'s paste framing
                    // matches, so the two agree on what a marker is. Inside a
                    // paste only the closing marker is framing: an opening one
                    // there is pasted text, and that framing stays in the paste
                    // across it (a paste cannot nest), so it is content here too.
                    let paste_marker = byte == b'~'
                        && len == 3
                        && match code {
                            Some(201) => true,
                            Some(200) => !paste,
                            _ => false,
                        };
                    if paste_marker {
                        return Escape::Ground;
                    }
                    if paste {
                        self.input_through();
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
                        self.input_through();
                    }
                    Escape::Ground
                }
                // Not a CSI after all: count what was sent as input and read
                // this byte afresh.
                _ => {
                    self.input_sequence();
                    self.ground(byte, in_paste)
                }
            },
            Escape::Ss3 => match byte {
                // A control byte, `ESC` included, is not an SS3 final: `ESC O`
                // was `Alt+O`, and this byte is read afresh — so a `Ctrl+U`
                // still clears and an `ESC` still opens its own sequence.
                0x00..=0x1f => {
                    self.input_sequence();
                    self.ground(byte, in_paste)
                }
                _ => {
                    self.input_through();
                    Escape::Ground
                }
            },
            Escape::Osc { len, esc } | Escape::Dcs { len, esc } => {
                let is_osc = matches!(self.escape, Escape::Osc { .. });
                let next = |len: u16, esc: Option<bool>| {
                    if is_osc {
                        Escape::Osc { len, esc }
                    } else {
                        Escape::Dcs { len, esc }
                    }
                };
                if let Some(string_user) = esc {
                    if byte == b'\\' {
                        return Escape::Ground;
                    }
                    // PR #1398 finding #17: the string was never a reply, so
                    // it is input for whoever sent it — exactly as the CSI
                    // fallthrough counts its sequence — and its trailing ESC,
                    // already attributed as an opener, starts the sequence
                    // this byte continues.
                    self.pending |= string_user;
                    self.escape = Escape::Esc { paste: false };
                    return self.step(byte, in_paste);
                }
                match byte {
                    // Either the start of the `ESC \` terminator or of a new
                    // sequence: park the string's credit, and read the ESC as
                    // an opener like any other until the next byte decides.
                    ESC => {
                        let string_user = self.seq_user;
                        self.open_sequence(false);
                        next(len, Some(string_user))
                    }
                    BEL if is_osc => Escape::Ground,
                    // A control byte inside a reply means it was never one.
                    0x00..=0x1f => {
                        self.input_sequence();
                        self.ground(byte, in_paste)
                    }
                    _ if len >= MAX_STRING_LEN => {
                        self.input_through();
                        Escape::Ground
                    }
                    _ => next(len + 1, None),
                }
            }
            Escape::X10 { remaining } => match byte {
                // Never a report after all: count what was sent as input and
                // read this byte afresh, as a control byte inside an OSC does.
                0x00..=0x1f => {
                    self.input_sequence();
                    self.ground(byte, in_paste)
                }
                _ if remaining > 1 => Escape::X10 {
                    remaining: remaining - 1,
                },
                _ => Escape::Ground,
            },
        }
    }

    /// The byte being fed is an `ESC` that opens a sequence of its own, sent by
    /// whoever sent it. Outside a paste it is not input by itself — a lone
    /// Escape edits nothing, and the sequence counts for whoever sends what
    /// follows — while inside a paste it is content.
    fn open_sequence(&mut self, in_paste: bool) {
        self.esc_user = self.by_user;
        self.seq_user = self.by_user && in_paste;
        self.fresh = true;
    }

    fn ground(&mut self, byte: u8, in_paste: bool) -> Escape {
        match byte {
            // Inside a paste an ESC is content — unless it opens the closing
            // marker, which `Escape::Esc { paste: true }` decides. Either way
            // it opens a sequence of its own, sent by whoever sent it.
            0x1b => {
                self.open_sequence(in_paste);
                Escape::Esc { paste: in_paste }
            }
            0x15 | 0x03 if !in_paste => {
                if self.by_user {
                    self.pending = false;
                }
                Escape::Ground
            }
            _ => {
                self.input_byte();
                Escape::Ground
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tracker: &mut DraftTracker, bytes: &[u8]) {
        feed_as(tracker, bytes, ByteOrigin::User);
    }

    /// [`feed`] for bytes `origin` wrote, outside any paste.
    fn feed_as(tracker: &mut DraftTracker, bytes: &[u8], origin: ByteOrigin) {
        let mut preceding = None;
        for &byte in bytes {
            let submits = crate::ui::user_byte_submits_input_box(preceding, byte);
            tracker.feed_byte(byte, submits, false, origin);
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
            tracker.feed_byte(byte, false, true, ByteOrigin::User);
            assert!(tracker.pending(), "{byte:#x}");
        }
    }

    /// Feed `bytes` wrapped in bracketed-paste markers, with the paste framing
    /// `crate::agent_pty`'s stream supplies: the state BEFORE each byte, so the
    /// opening marker is outside the paste and the closing one inside it.
    fn feed_paste(tracker: &mut DraftTracker, bytes: &[u8]) {
        for &byte in b"\x1b[200~" {
            tracker.feed_byte(byte, false, false, ByteOrigin::User);
        }
        for &byte in bytes {
            tracker.feed_byte(byte, false, true, ByteOrigin::User);
        }
        for &byte in b"\x1b[201~" {
            tracker.feed_byte(byte, false, true, ByteOrigin::User);
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
                tracker.feed_byte(byte, false, in_paste, ByteOrigin::User);
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

    /// PR #1398 re-review: inside a paste only the CLOSING marker is framing.
    /// A literal `ESC[200~` in the pasted text is content — a paste cannot
    /// nest, so `crate::agent_pty`'s stream stays in the paste across it — and
    /// an empty box holding just that text holds a draft.
    #[test]
    fn a_pasted_opening_marker_is_content() {
        let mut tracker = DraftTracker::default();
        feed_paste(&mut tracker, b"\x1b[200~");
        assert!(tracker.pending(), "a pasted ESC[200~ did not set the bit");
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

    /// Our own SUBMIT write is payload + CR, fed as the deck's bytes: the
    /// payload sets nothing and the CR submits the box, the user's draft
    /// included.
    #[test]
    fn our_own_submit_clears_the_bit() {
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"draft");
        feed_as(&mut tracker, b"PAYLOAD", ByteOrigin::Deck);
        assert!(tracker.pending(), "our payload does not clear the draft");
        feed_as(&mut tracker, b"\r", ByteOrigin::Deck);
        assert!(!tracker.pending());
    }

    /// PR #1398 finding #16: the deck's bytes move the parser but never set
    /// the bit — neither as text nor as paste content nor as a newline.
    #[test]
    fn deck_bytes_never_set_the_bit() {
        for bytes in [
            &b"PAYLOAD"[..],
            b"PAYLOAD\n",
            b"\x1b[200~one\ntwo\x1b[201~",
            b"\x7f\x7f",
        ] {
            let mut tracker = DraftTracker::default();
            feed_as(&mut tracker, bytes, ByteOrigin::Deck);
            assert!(!tracker.pending(), "{bytes:?}");
        }
    }

    /// PR #1398 finding #16: a sequence the user began and a deck byte ended
    /// is decided by who sent the bytes after the `ESC`. A lone Escape and
    /// then our text is not a draft; `Alt+]` and the text the user kept typing
    /// is one, even though our LF is what ended the would-be reply.
    #[test]
    fn a_users_sequence_ended_by_a_deck_byte_counts_only_the_users_part() {
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"\x1b");
        feed_as(&mut tracker, b"NOTICE\n", ByteOrigin::Deck);
        assert!(!tracker.pending(), "a lone Escape before our notice");

        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"\x1b]typed");
        assert!(!tracker.pending(), "still a would-be reply");
        feed_as(&mut tracker, b"NOTICE\n", ByteOrigin::Deck);
        assert!(tracker.pending(), "the user's Alt+] and text were dropped");

        // A deck `Ctrl+U` is not the user clearing their draft.
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"draft");
        feed_as(&mut tracker, b"\x15", ByteOrigin::Deck);
        assert!(tracker.pending());
    }

    /// Issue #544 (review of finding #16, observation 1): in `ESC ESC` the
    /// first `ESC` is a key of its own, and it is credited to whoever sent
    /// THAT byte — not to the sender of the second, which starts a fresh
    /// sequence of its own. A deck `ESC` (a partial write) and then the
    /// user's is therefore not a draft, two user `ESC`s still are, and the
    /// user's lone Escape before our paste framing stays one that edits
    /// nothing, as it does before our text.
    #[test]
    fn a_double_escape_credits_the_first_escape_to_its_own_sender() {
        let mut tracker = DraftTracker::default();
        feed_as(&mut tracker, b"\x1b", ByteOrigin::Deck);
        feed(&mut tracker, b"\x1b");
        assert!(
            !tracker.pending(),
            "the deck's ESC was credited to the user"
        );

        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"\x1b\x1b");
        assert!(
            tracker.pending(),
            "the user's double Escape stopped counting"
        );

        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"\x1b");
        feed_as(&mut tracker, b"\x1b[200~NOTICE\x1b[201~", ByteOrigin::Deck);
        assert!(!tracker.pending(), "a lone Escape before our paste");
    }

    /// Sweep (PR #1398): CSI intermediate bytes are bounded like parameter
    /// bytes. Past the bound the sequence is input, whatever its final byte —
    /// otherwise `Alt+[`, a run of spaces and a report-shaped letter swallow
    /// the lot without setting the bit.
    #[test]
    fn an_overlong_csi_intermediate_run_is_input() {
        let mut bytes = b"\x1b[".to_vec();
        bytes.extend(std::iter::repeat_n(b' ', usize::from(MAX_CSI_LEN) + 8));
        bytes.push(b'n');
        assert!(pending_after(&bytes));
    }

    /// PR #1398 finding #15: a daemon submit resets the escape parser exactly
    /// as a user submit does, so a stale lone `ESC` cannot turn later typing
    /// into an OSC report. Since finding #16 that is the parser reading our
    /// bytes rather than a reset of its own.
    #[test]
    fn a_daemon_submit_resets_a_stale_escape_prefix() {
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"\x1b");
        feed_as(&mut tracker, b"PAYLOAD\r", ByteOrigin::Deck);
        feed(&mut tracker, b"]abc");
        assert!(tracker.pending(), "typing after a daemon submit is a draft");
    }

    /// PR #1398 finding #17: an unterminated OSC or DCS string whose last
    /// byte is `ESC`, followed by anything but the `\` of an `ESC \`
    /// terminator, was never a reply. The string is input for whoever sent
    /// it — so the user's `Alt+]` and what they typed after it count even
    /// when a deck byte is what ends it — and the trailing `ESC` opens a new
    /// sequence of its own.
    #[test]
    fn an_unterminated_string_ended_by_a_deck_byte_credits_the_users_string() {
        for opener in [&b"\x1b]"[..], b"\x1bP"] {
            let mut typed = opener.to_vec();
            typed.extend_from_slice(b"abc\x1b");
            for deck in [&b"NOTICE\n"[..], b"\x1b[200~NOTICE\x1b[201~"] {
                let mut tracker = DraftTracker::default();
                feed(&mut tracker, &typed);
                assert!(!tracker.pending(), "still a would-be reply");
                feed_as(&mut tracker, deck, ByteOrigin::Deck);
                assert!(
                    tracker.pending(),
                    "the user's {typed:?} was dropped when the deck wrote {deck:?}"
                );
            }
        }
        // The same string, terminated, is a reply and sets nothing.
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"\x1b]abc\x1b");
        feed_as(&mut tracker, b"\\", ByteOrigin::Deck);
        assert!(!tracker.pending());
    }

    /// Arm-by-arm sweep after finding #17: a sequence counts for whoever sent
    /// its bytes AFTER the `ESC`. A lone Escape and then deck text is not a
    /// draft whatever the text's first byte happens to be — before this, text
    /// starting with `[`, `]`, `P` or `O` was parsed as a sequence the user's
    /// `ESC` opened and credited to them, while any other text was not.
    #[test]
    fn a_lone_escape_before_deck_text_is_not_a_draft_whatever_the_text_starts_with() {
        for deck in [
            &b"NOTICE\n"[..],
            b"[1] done\n",
            b"]done\n",
            b"Pane 2 finished\n",
            b"OK\n",
        ] {
            let mut tracker = DraftTracker::default();
            feed(&mut tracker, b"\x1b");
            feed_as(&mut tracker, deck, ByteOrigin::Deck);
            assert!(!tracker.pending(), "a lone Escape before {deck:?}");
        }
        // The user's own bytes after an ESC still count, whoever sent the ESC.
        for (esc, rest) in [(ByteOrigin::User, &b"[A"[..]), (ByteOrigin::Deck, b"[A")] {
            let mut tracker = DraftTracker::default();
            feed_as(&mut tracker, b"\x1b", esc);
            feed(&mut tracker, rest);
            assert!(tracker.pending(), "{esc:?} ESC, then the user's {rest:?}");
        }
    }

    /// Arm-by-arm sweep after finding #17: a control byte (`ESC` included) is
    /// not the final byte of an SS3 key. `ESC O` was `Alt+O`, and the control
    /// byte is read afresh — so the user's `Ctrl+U` after it still clears,
    /// and an `ESC` after it still opens the sequence it starts.
    #[test]
    fn an_ss3_prefix_does_not_swallow_the_control_byte_after_it() {
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"draft\x1bO\x15");
        assert!(!tracker.pending(), "Ctrl+U after Alt+O did not clear");

        // A lone Escape, deck text starting `O`, then the user's empty paste:
        // nothing the user sent is content.
        let mut tracker = DraftTracker::default();
        feed(&mut tracker, b"\x1b");
        feed_as(&mut tracker, b"O", ByteOrigin::Deck);
        feed(&mut tracker, b"\x1b[200~\x1b[201~");
        assert!(!tracker.pending(), "the user's paste framing set the bit");
    }

    /// Arm-by-arm sweep after finding #17: every raw byte of a real X10 mouse
    /// report is 32 plus a value, so a control byte after `ESC [ M` means it
    /// was never one — the sequence is input, as an aborted OSC is, and the
    /// control byte is read afresh.
    #[test]
    fn an_x10_prefix_does_not_swallow_a_control_byte() {
        for prefix in [&b"\x1b[M"[..], b"\x1b[M ", b"\x1b[M  "] {
            let mut bytes = b"draft".to_vec();
            bytes.extend_from_slice(prefix);
            bytes.push(0x15);
            assert!(!pending_after(&bytes), "Ctrl+U after {prefix:?}");
        }
        // A complete report still sets nothing, as before.
        assert!(!pending_after(b"\x1b[M !!"));
    }

    /// Who wrote the prefix (`A`) and who wrote the interrupting bytes (`B`).
    const ORIGINS: [(ByteOrigin, ByteOrigin); 4] = [
        (ByteOrigin::User, ByteOrigin::User),
        (ByteOrigin::User, ByteOrigin::Deck),
        (ByteOrigin::Deck, ByteOrigin::User),
        (ByteOrigin::Deck, ByteOrigin::Deck),
    ];

    #[derive(Clone, Copy, Debug)]
    enum Clears {
        No,
        /// The interrupting byte submits, whoever sent it.
        Submit,
        /// The interrupting byte is a clear key, which clears only from the user.
        UserKey,
    }

    /// What an interruption does to the bit, stated as the invariants:
    /// `prefix` — the prefix is input (text, or a sequence that ended up input:
    /// completed as a key, abandoned, overlong, aborted), counting for `A`
    /// when `A` is the user; `byte` — the interrupting bytes are input for
    /// `B`; `esc_esc` — `ESC ESC`, where the first is a lone Escape that
    /// counts only when the user sent it AND the user sent the next. A prefix
    /// that is a lone `ESC` is never input by itself, and framing or a report
    /// is never input at all.
    #[derive(Clone, Copy, Debug)]
    struct Rule {
        prefix: bool,
        byte: bool,
        esc_esc: bool,
        clears: Clears,
    }

    const fn rule(prefix: bool, byte: bool) -> Rule {
        Rule {
            prefix,
            byte,
            esc_esc: false,
            clears: Clears::No,
        }
    }
    const NOTHING: Rule = rule(false, false);
    const PREFIX: Rule = rule(true, false);
    const BYTE: Rule = rule(false, true);
    const BOTH: Rule = rule(true, true);
    const ESC_ESC: Rule = Rule {
        esc_esc: true,
        ..NOTHING
    };
    const SUBMIT: Rule = Rule {
        clears: Clears::Submit,
        ..NOTHING
    };
    const CLEAR: Rule = Rule {
        clears: Clears::UserKey,
        ..NOTHING
    };
    const CLEAR_AFTER_PREFIX: Rule = Rule {
        clears: Clears::UserKey,
        ..PREFIX
    };

    impl Rule {
        fn expect(self, a: ByteOrigin, b: ByteOrigin, drafted: bool) -> bool {
            let (a, b) = (a == ByteOrigin::User, b == ByteOrigin::User);
            match self.clears {
                Clears::Submit => return false,
                Clears::UserKey if b => return false,
                _ => {}
            }
            drafted || (self.prefix && a) || (self.byte && b) || (self.esc_esc && a && b)
        }
    }

    /// A parser state, the prefix that enters it, and each interruption of it
    /// with the [`Rule`] it must follow.
    type StateRow = (&'static str, Vec<u8>, Vec<(&'static [u8], Rule)>);

    /// Feed `chunks` through the real stream (paste framing, submit scan,
    /// draft parser) and report the bit and whether a paste is open.
    fn run_stream(chunks: &[(&[u8], ByteOrigin)]) -> (bool, bool) {
        let mut stream = crate::agent_pty::DraftTestStream::default();
        for (bytes, origin) in chunks {
            for &byte in *bytes {
                stream.feed_byte(byte, *origin);
            }
        }
        (stream.draft().pending(), stream.in_paste())
    }

    /// Arm-by-arm sweep after finding #17: every parser state that a byte can
    /// interrupt, entered by a prefix `A` wrote and interrupted or ended by
    /// bytes `B` wrote, for all four combinations of `A` and `B`, starting
    /// from an empty box and from one holding the user's draft — checked
    /// against the invariants in [`Rule`]. Inside a paste the opening marker
    /// is the deck's (framing, which sets nothing whoever sends it) and every
    /// state is content.
    #[test]
    fn every_interruptible_state_honours_the_invariants_for_every_sender_pair() {
        const X: &[u8] = b"x";
        const FRAME: &[u8] = b"\x1b[200~\x1b[201~";
        const CLOSE: &[u8] = b"\x1b[201~";
        const LF: &[u8] = b"\n";
        const CU: &[u8] = b"\x15";
        const CC: &[u8] = b"\x03";
        const CR: &[u8] = b"\r";
        let mut csi_param_bound = b"\x1b[".to_vec();
        csi_param_bound.extend(std::iter::repeat_n(b'1', usize::from(MAX_CSI_LEN)));
        let mut csi_inter_bound = b"\x1b[".to_vec();
        csi_inter_bound.extend(std::iter::repeat_n(b' ', usize::from(MAX_CSI_LEN)));
        let mut osc_bound = b"\x1b]".to_vec();
        osc_bound.extend(std::iter::repeat_n(b'a', usize::from(MAX_STRING_LEN)));

        // `x_rule`: a printable byte continues the string, or, at the bound,
        // makes it overlong input.
        let string_arms = |x_rule: Rule, terminator: &'static [u8], at_terminator: Rule| {
            vec![
                (X, x_rule),
                (terminator, at_terminator),
                (b"\x1b\\".as_slice(), NOTHING),
                (FRAME, PREFIX),
                (LF, BOTH),
                (CU, CLEAR_AFTER_PREFIX),
                (CC, CLEAR_AFTER_PREFIX),
                (CR, SUBMIT),
            ]
        };
        // An unterminated string's trailing ESC, then anything but `\`: the
        // string is input for `A`, and `ESC <byte>` is a new sequence of `B`'s
        // (or, for `ESC ESC`, a lone Escape of `B`'s).
        let string_esc_arms = vec![
            (X, BOTH),
            (b"\\".as_slice(), NOTHING),
            (FRAME, PREFIX),
            (b"[A".as_slice(), BOTH),
            (LF, BOTH),
            // `ESC Ctrl+U` and `ESC CR` are `Alt+` keys: input, and neither
            // clears nor submits.
            (CU, BOTH),
            (CR, BOTH),
        ];
        let aborted_arms = |key: &'static [u8], at_key: Rule| {
            vec![
                (X, if at_key.byte { BOTH } else { NOTHING }),
                (key, at_key),
                (FRAME, PREFIX),
                (LF, BOTH),
                (CU, CLEAR_AFTER_PREFIX),
                (CC, CLEAR_AFTER_PREFIX),
                (CR, SUBMIT),
            ]
        };

        let outside: Vec<StateRow> = vec![
            (
                "ground",
                Vec::new(),
                vec![
                    (X, BYTE),
                    (FRAME, NOTHING),
                    (LF, BYTE),
                    (CU, CLEAR),
                    (CC, CLEAR),
                    (CR, SUBMIT),
                ],
            ),
            (
                "ESC",
                b"\x1b".to_vec(),
                vec![
                    (X, BYTE),
                    (FRAME, ESC_ESC),
                    (LF, BYTE),
                    // The accepted ambiguity: `ESC Ctrl+U` is `Alt+Ctrl+U`.
                    (CU, BYTE),
                    (CR, BYTE),
                    (b"[A", BYTE),
                    (b"[1] done\n", BYTE),
                    (b"]done\n", BYTE),
                    (b"Pdone\n", BYTE),
                    (b"OK", BYTE),
                ],
            ),
            (
                "CSI params",
                b"\x1b[1".to_vec(),
                vec![
                    (X, BOTH),
                    (b"~", BOTH),
                    (b"n", NOTHING),
                    (FRAME, PREFIX),
                    (LF, BOTH),
                    (CU, CLEAR_AFTER_PREFIX),
                    (CC, CLEAR_AFTER_PREFIX),
                    (CR, SUBMIT),
                ],
            ),
            (
                "CSI intermediates",
                b"\x1b[1 ".to_vec(),
                aborted_arms(b"q", BOTH),
            ),
            (
                "CSI private",
                b"\x1b[?1".to_vec(),
                aborted_arms(b"c", NOTHING),
            ),
            (
                "CSI params at bound",
                csi_param_bound,
                aborted_arms(b"1", BOTH),
            ),
            (
                "CSI intermediates at bound",
                csi_inter_bound,
                aborted_arms(b" ", BOTH),
            ),
            (
                "OSC",
                b"\x1b]ab".to_vec(),
                string_arms(NOTHING, b"\x07", NOTHING),
            ),
            ("OSC at bound", osc_bound, string_arms(BOTH, b"a", BOTH)),
            (
                "OSC + ESC",
                b"\x1b]ab\x1b".to_vec(),
                string_esc_arms.clone(),
            ),
            // BEL ends an OSC, not a DCS: in a DCS it is a control byte.
            (
                "DCS",
                b"\x1bPab".to_vec(),
                string_arms(NOTHING, b"\x07", BOTH),
            ),
            ("DCS + ESC", b"\x1bPab\x1b".to_vec(), string_esc_arms),
            ("X10", b"\x1b[M ".to_vec(), aborted_arms(b"!", NOTHING)),
            ("SS3", b"\x1bO".to_vec(), aborted_arms(b"P", BOTH)),
        ];

        // Inside a paste every state is content and nothing clears.
        let pasted = |prefix_is_content: bool| {
            let p = |rule: Rule| Rule {
                prefix: prefix_is_content,
                ..rule
            };
            vec![
                (X, p(BYTE)),
                (CLOSE, p(NOTHING)),
                (LF, p(BYTE)),
                (CU, p(BYTE)),
                (CC, p(BYTE)),
                (CR, p(BYTE)),
                (b"\x1b[200~".as_slice(), p(BYTE)),
            ]
        };
        let inside: Vec<StateRow> = vec![
            ("paste: ground", Vec::new(), pasted(false)),
            ("paste: ESC", b"\x1b".to_vec(), pasted(true)),
            ("paste: CSI params", b"\x1b[1".to_vec(), pasted(true)),
            (
                "paste: CSI intermediates",
                b"\x1b[1 ".to_vec(),
                pasted(true),
            ),
            ("paste: CSI private", b"\x1b[?1".to_vec(), pasted(true)),
            ("paste: OSC", b"\x1b]ab".to_vec(), pasted(true)),
            ("paste: OSC + ESC", b"\x1b]ab\x1b".to_vec(), pasted(true)),
            ("paste: DCS", b"\x1bPab".to_vec(), pasted(true)),
            ("paste: DCS + ESC", b"\x1bPab\x1b".to_vec(), pasted(true)),
            ("paste: X10", b"\x1b[M ".to_vec(), pasted(true)),
            ("paste: SS3", b"\x1bO".to_vec(), pasted(true)),
        ];

        let mut failures = Vec::new();
        for (in_paste, rows) in [(false, &outside), (true, &inside)] {
            for (state, prefix, arms) in rows {
                for (interrupt, rule) in arms {
                    for (a, b) in ORIGINS {
                        for drafted in [false, true] {
                            let mut chunks: Vec<(&[u8], ByteOrigin)> = Vec::new();
                            if drafted {
                                chunks.push((b"d", ByteOrigin::User));
                            }
                            if in_paste {
                                chunks.push((b"\x1b[200~", ByteOrigin::Deck));
                            }
                            chunks.push((prefix, a));
                            chunks.push((interrupt, b));
                            let (pending, paste_open) = run_stream(&chunks);
                            let expected = rule.expect(a, b, drafted);
                            // Framing agrees too: the paste is open afterwards
                            // exactly when the row opened one and did not close it.
                            let expected_open = in_paste && *interrupt != CLOSE;
                            if pending != expected || paste_open != expected_open {
                                failures.push(format!(
                                    "{state} + {interrupt:?}: A={a:?} B={b:?} drafted={drafted}: \
                                     pending {pending} (want {expected}), paste open \
                                     {paste_open} (want {expected_open})"
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// A small deterministic generator, so the randomized sweep below needs
    /// no new dependency and reproduces exactly from its seed.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Arm-by-arm sweep after finding #17: random mixed-origin streams built
    /// from the bytes every arm branches on, checked byte by byte against the
    /// invariants that need no oracle:
    ///
    /// 1. The bit goes from clear to set only on a byte of a sequence holding
    ///    a USER byte — counted from where the parser last left ground, and
    ///    not counting that sequence's opening `ESC` outside a paste (a lone
    ///    Escape is not an edit by itself). Deck bytes alone never set it.
    /// 3. The bit goes from set to clear only on a byte that submits, or on
    ///    the user's `Ctrl+U` / `Ctrl+C` outside a paste.
    #[test]
    fn random_mixed_origin_streams_honour_the_invariants() {
        const TOKENS: [&[u8]; 34] = [
            b"\x1b",
            b"[",
            b"]",
            b"P",
            b"O",
            b"M",
            b"\\",
            b"\x07",
            b"\r",
            b"\n",
            b"\x15",
            b"\x03",
            b"2",
            b"0",
            b"1",
            b"~",
            b";",
            b"?",
            b" ",
            b"$",
            b"x",
            b"A",
            b"n",
            b"y",
            b"\x7f",
            b"\t",
            b"\x1b[200~",
            b"\x1b[201~",
            b"\x1b[I",
            b"\x1b[<0;1;2M",
            b"\x1b]10;x\x07",
            b"\x1b\\",
            b"\x1b\r",
            b"\x1bOP",
        ];
        for seed in 1..=4000u64 {
            let mut rng = XorShift(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut stream = crate::agent_pty::DraftTestStream::default();
            let mut fed: Vec<(u8, ByteOrigin, bool)> = Vec::new();
            let mut window_start = 0;
            for _ in 0..48 {
                let token = TOKENS[rng.below(TOKENS.len())];
                let origin = if rng.below(2) == 0 {
                    ByteOrigin::User
                } else {
                    ByteOrigin::Deck
                };
                for &byte in token {
                    let before = stream.draft().pending();
                    let was_ground = stream.draft().escape == Escape::Ground;
                    let in_paste = stream.in_paste();
                    if was_ground {
                        window_start = fed.len();
                    }
                    fed.push((byte, origin, in_paste));
                    let submits = stream.feed_byte(byte, origin);
                    let after = stream.draft().pending();
                    let history = || {
                        fed.iter()
                            .map(|(b, o, _)| {
                                format!(
                                    "{b:#04x}{}",
                                    if *o == ByteOrigin::User { "u" } else { "d" }
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    };
                    if !before && after {
                        let window = &fed[window_start..];
                        let credited = window.iter().enumerate().any(|(i, &(b, o, p))| {
                            o == ByteOrigin::User && !(i == 0 && b == 0x1b && !p)
                        });
                        assert!(
                            credited,
                            "seed {seed}: the bit was set with no user byte in the sequence: {}",
                            history()
                        );
                    }
                    if before && !after {
                        let user_clear =
                            origin == ByteOrigin::User && matches!(byte, 0x15 | 0x03) && !in_paste;
                        assert!(
                            submits || user_clear,
                            "seed {seed}: the bit was cleared by neither a submit nor a user clear: {}",
                            history()
                        );
                    }
                }
            }
        }
    }
}
