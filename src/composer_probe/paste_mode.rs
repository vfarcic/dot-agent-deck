//! PRD #1666 part 3 (Q35-02, core §4.4 and §12.6): whether an agent has
//! bracketed paste on, read from what it writes, and the output hook that
//! reads it.
//!
//! [`OutputHook::on_output`] is handed every byte a generation writes, under
//! the output lock and before the bytes are published to any reader (C1's
//! I-2 and the output half of I-3); its one call site is
//! `AgentBus::push_timed`. It feeds [`PasteModeTracker`] and, from the same
//! pass over the bytes, hands the generation's input epoch the queries,
//! `?2026h` and report modes it saw (Q35-06, `super::epoch::OutputNote`),
//! taking the epoch's lock once for a push that has any, after advancing the
//! generation's output offset by the push's length. No decision reads the
//! tracker yet; of what the notes feed, only the query ledger is read, by a
//! closed fence deciding whether a reply passes ahead (Q35-15), and only the
//! `e2e` build closes one. The hook changes no byte that is published, kept
//! or replayed. It is not free: it is one more pass over the bytes while the
//! output lock is held, before they are published.
//!
//! The tracker is separate from the ring's `ReplayModes` on purpose (plan PA
//! U3-01): it reads the output through [`Framing::output`], which reads
//! strings, controls inside escapes and intermediate bytes as the `vte` state
//! machine does, with the exceptions its documentation names, while the replay
//! preamble keeps its own scanner unchanged.
//!
//! **What it follows, and where it differs from `vt100`.** Mode 2004 is set
//! by `CSI ? … h` and reset by `CSI ? … l` when a parameter is exactly 2004,
//! and reset by RIS (`ESC c`). Parameters are read as the `vte` crate under
//! `vt100` keeps them: the first 32 values, sub-parameters included, a
//! parameter cut short at the 32nd keeping the values it got. Intermediate
//! bytes after the `?` don't stop it, as they don't stop `vt100`. The
//! difference known and tested: a CSI with more than [`MAX_SEQUENCE_BYTES`]
//! parameter and intermediate bytes is malformed here and changes nothing,
//! where `vt100` still applies it. That isn't a conservative direction: an
//! overlong enable leaves the tracker off where `vt100` is on, and an
//! overlong disable leaves it on where `vt100` is off.
//!
//! **What matching `vt100` does and doesn't establish.** `vt100` is this
//! repository's consumer, so equality with it is what the tests check. It is
//! not evidence of how xterm, xterm.js or the outer terminal read the same
//! bytes, nor of how an agent's input reader treats a paste. Before anything
//! uses this state to decide (core plan Q35-16/20), that caller has to handle
//! a stale on as well as a stale off, and keep a candidate whenever the mode
//! it needs isn't established.
//!
//! [`MAX_SEQUENCE_BYTES`]: super::framing::MAX_SEQUENCE_BYTES

use std::sync::Arc;
use std::time::Instant;

use super::epoch::{GenerationInput, OutputNote, output_notes};
use super::framing::{Csi, Event, Framing, Token};
use super::observe::BellSlot;
use super::segments::{Seen, seen_ending_at};
#[cfg(test)]
use super::stamp_park::StampPoint;

/// DEC private mode 2004: bracketed paste.
const BRACKETED_PASTE: u32 = 2004;

/// How many parameter values, sub-parameters included, `vte` keeps.
const VTE_MAX_PARAMS: usize = 32;

/// What core reads from a generation's output, fed every byte it writes.
#[derive(Debug, Clone, Default)]
pub struct OutputHook {
    paste_mode: PasteModeTracker,
    input: Arc<GenerationInput>,
    /// The generation's observer, once it has one (Q35-14b): rung for a
    /// child scan when the output turns bracketed paste off.
    bell: BellSlot,
    /// How many bytes it has read, `None` once that can't advance: the drain
    /// frontier's `o` (Q35-20a, A5 §3.2), read under the output lock.
    fed: Option<u64>,
}

impl OutputHook {
    /// A hook that notes what it reads into `input`, the generation's epoch.
    pub fn new(input: Arc<GenerationInput>) -> Self {
        Self {
            paste_mode: PasteModeTracker::default(),
            input,
            bell: BellSlot::default(),
            fed: Some(0),
        }
    }

    /// Where the generation's observer puts its bell.
    pub fn bell_slot(&self) -> BellSlot {
        Arc::clone(&self.bell)
    }

    /// Read the next bytes the agent wrote.
    pub fn on_output(&mut self, bytes: &[u8]) {
        let mut notes: Vec<OutputNote> = Vec::new();
        let mut paste_off = false;
        // Q35-20 (R-U2-02): the return markers are read here, in this
        // framing and this count, both from the generation's birth: the same
        // coordinate its triggers' offsets and drain frontiers are in, and a
        // framing that was never started mid-sequence.
        let mut markers: Vec<Seen> = Vec::new();
        let base = self.fed;
        self.paste_mode.feed_observing(bytes, |token, modes, at| {
            if let Event::Csi(csi) = token.event {
                if csi.final_byte == b'l' && modes.contains(&BRACKETED_PASTE) {
                    paste_off = true;
                }
                if let Some(seen) = base
                    .and_then(|base| base.checked_add(at as u64))
                    .and_then(|end| seen_ending_at(token, &csi, end))
                {
                    markers.push(seen);
                }
            }
            output_notes(token, modes, &mut notes)
        });
        // The push's end is the offset its queries are noted at (Q35-15):
        // one atomic add per push, outside the epoch's lock.
        self.input.advance_output(bytes.len());
        self.fed = self.fed.and_then(|fed| {
            u64::try_from(bytes.len())
                .ok()
                .and_then(|n| fed.checked_add(n))
        });
        // The stamp is taken after the push's offset advanced, so it is no
        // earlier than the offset's move (Q84-3).
        #[cfg(test)]
        self.input.feeder_at(StampPoint::OutputAdvanced);
        let now = Instant::now();
        // Test seam (Q84-3): a racing caller may take the generation's lock
        // with a later stamp before this one is published.
        #[cfg(test)]
        self.input.feeder_at(StampPoint::OutputStamped);
        if !notes.is_empty() {
            self.input.on_output(&notes, now);
        }
        #[cfg(test)]
        self.input.feeder_at(StampPoint::OutputNoted);
        // Q35-20: an installed generation's segments read the push's return
        // markers, after the notes, under the same output lock. Nothing
        // without segments attached.
        if !markers.is_empty() {
            self.input.on_return_markers(&markers, now);
        }
        // An agent writes `?2004l` as it runs its editor: scan for the
        // helper while it is starting (R5 §3.3). Ringing takes no lock.
        if paste_off && let Some(bell) = self.bell.get() {
            bell.ring_scan();
        }
    }

    /// Every byte it has read, counted: the offset a drain frontier taken now
    /// is at. `None` once the count overflowed, which is sticky, and every
    /// frontier is then missing (A5 §3.2).
    pub fn fed(&self) -> Option<u64> {
        self.fed
    }

    /// Test seam: start the count at `fed`, to reach exhaustion with a
    /// reduced bound. The Linux reader's frontier tests are its only users.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn set_fed(&mut self, fed: u64) {
        self.fed = Some(fed);
    }

    /// The agent's bracketed-paste mode, as far as its output says.
    pub fn paste_mode(&self) -> &PasteModeTracker {
        &self.paste_mode
    }
}

/// Mode 2004 as an agent's output sets and resets it. Off until the output
/// turns it on, as a terminal starts.
#[derive(Debug, Clone)]
pub struct PasteModeTracker {
    framing: Framing,
    on: bool,
}

impl Default for PasteModeTracker {
    fn default() -> Self {
        Self {
            framing: Framing::output(),
            on: false,
        }
    }
}

impl PasteModeTracker {
    /// Read the next bytes of the output, in order, however they were split.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.feed_observing(bytes, |_, _, _| {});
    }

    /// [`Self::feed`], handing `observe` every token, for a `CSI ? … h` or
    /// `l` the whole parameter values `vte` keeps (empty otherwise), and the
    /// index in `bytes` of the byte that completed it.
    pub(super) fn feed_observing(
        &mut self,
        bytes: &[u8],
        mut observe: impl FnMut(&Token<'_>, &[u32], usize),
    ) {
        let Self { framing, on } = self;
        for (at, &byte) in bytes.iter().enumerate() {
            framing.feed(byte, |token| {
                let modes = match token.event {
                    Event::Csi(csi)
                        if csi.private == Some(b'?') && matches!(csi.final_byte, b'h' | b'l') =>
                    {
                        kept_modes(&csi)
                    }
                    _ => Vec::new(),
                };
                match token.event {
                    Event::Csi(csi) if modes.contains(&BRACKETED_PASTE) => {
                        *on = csi.final_byte == b'h';
                    }
                    Event::Escape(b'c') => *on = false,
                    _ => {}
                }
                observe(&token, &modes, at);
            });
        }
    }

    /// Whether the agent's bracketed paste is on.
    pub fn on(&self) -> bool {
        self.on
    }
}

/// The parameters `vte` keeps that are one whole value, with no
/// sub-parameter, in order.
fn kept_modes(csi: &Csi<'_>) -> Vec<u32> {
    let mut modes = Vec::new();
    let mut kept = 0;
    for param in csi.params() {
        let room = VTE_MAX_PARAMS - kept;
        if room == 0 {
            break;
        }
        let mut values = param.subparams().take(room);
        let first = values.next().flatten();
        let more = values.count();
        if more == 0
            && let Some(mode) = first
        {
            modes.push(mode);
        }
        kept += 1 + more;
    }
    modes
}
