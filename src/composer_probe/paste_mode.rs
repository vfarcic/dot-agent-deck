//! PRD #1666 part 3 (Q35-02, core §4.4 and §12.6): whether an agent has
//! bracketed paste on, read from what it writes, and the output hook that
//! reads it.
//!
//! [`OutputHook::on_output`] is handed every byte a generation writes, under
//! the output lock and before the bytes are published to any reader (C1's
//! I-2 and the output half of I-3); its one call site is
//! `AgentBus::push_timed`. Today it feeds only [`PasteModeTracker`], and
//! nothing reads the tracker yet: the hook changes no byte that is published,
//! kept or replayed, and no decision. It is not free: it is one more pass
//! over the bytes while the output lock is held, before they are published.
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

use super::framing::{Csi, Event, Framing};

/// DEC private mode 2004: bracketed paste.
const BRACKETED_PASTE: u32 = 2004;

/// How many parameter values, sub-parameters included, `vte` keeps.
const VTE_MAX_PARAMS: usize = 32;

/// What core reads from a generation's output, fed every byte it writes.
#[derive(Debug, Clone, Default)]
pub struct OutputHook {
    paste_mode: PasteModeTracker,
}

impl OutputHook {
    /// Read the next bytes the agent wrote.
    pub fn on_output(&mut self, bytes: &[u8]) {
        self.paste_mode.feed(bytes);
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
        let Self { framing, on } = self;
        for &byte in bytes {
            framing.feed(byte, |token| match token.event {
                Event::Csi(csi)
                    if csi.private == Some(b'?')
                        && matches!(csi.final_byte, b'h' | b'l')
                        && names_bracketed_paste(&csi) =>
                {
                    *on = csi.final_byte == b'h';
                }
                Event::Escape(b'c') => *on = false,
                _ => {}
            });
        }
    }

    /// Whether the agent's bracketed paste is on.
    pub fn on(&self) -> bool {
        self.on
    }
}

/// Whether a parameter `vte` keeps is exactly 2004, with no sub-parameter.
fn names_bracketed_paste(csi: &Csi<'_>) -> bool {
    let mut kept = 0;
    for param in csi.params() {
        let room = VTE_MAX_PARAMS - kept;
        if room == 0 {
            return false;
        }
        let mut values = param.subparams().take(room);
        let first = values.next().flatten();
        let more = values.count();
        if first == Some(BRACKETED_PASTE) && more == 0 {
            return true;
        }
        kept += 1 + more;
    }
    false
}
