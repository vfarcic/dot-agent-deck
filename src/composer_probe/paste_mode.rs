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
//! taking the epoch's lock once for a push that has any. No decision reads
//! the tracker or those notes yet: the hook changes no byte that is
//! published, kept or replayed, and no decision. It is not free: it is one
//! more pass over the bytes while the output lock is held, before they are
//! published.
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
}

impl OutputHook {
    /// A hook that notes what it reads into `input`, the generation's epoch.
    pub fn new(input: Arc<GenerationInput>) -> Self {
        Self {
            paste_mode: PasteModeTracker::default(),
            input,
            bell: BellSlot::default(),
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
        self.paste_mode.feed_observing(bytes, |token, modes| {
            if let Event::Csi(csi) = token.event
                && csi.final_byte == b'l'
                && modes.contains(&BRACKETED_PASTE)
            {
                paste_off = true;
            }
            output_notes(token, modes, &mut notes)
        });
        if !notes.is_empty() {
            self.input.on_output(&notes, Instant::now());
        }
        // An agent writes `?2004l` as it runs its editor: scan for the
        // helper while it is starting (R5 §3.3). Ringing takes no lock.
        if paste_off && let Some(bell) = self.bell.get() {
            bell.ring_scan();
        }
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
        self.feed_observing(bytes, |_, _| {});
    }

    /// [`Self::feed`], handing `observe` every token and, for a `CSI ? … h`
    /// or `l`, the whole parameter values `vte` keeps (empty otherwise).
    pub(super) fn feed_observing(
        &mut self,
        bytes: &[u8],
        mut observe: impl FnMut(&Token<'_>, &[u32]),
    ) {
        let Self { framing, on } = self;
        for &byte in bytes {
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
                observe(&token, &modes);
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
