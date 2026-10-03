//! DEC private-mode state carried by a stream of PTY output.
//!
//! Two readers need the same facts out of an agent's byte stream. The TUI's
//! pane task derives whether the child asked for SGR mouse reports (PRD #611
//! M3), and the daemon's output ring has to know which modes are in force at
//! its first byte, so a replay rebuilt from that ring starts where the agent
//! actually is (issue #1537). Both parse the same sequences with the same
//! cross-chunk carry, so the parser lives here once.
//!
//! The model is the repo's own `vt100` 0.16.2, deliberately: what matters is
//! what the deck's parser will make of these bytes, not what some other
//! terminal would.

/// Upper bound on the bytes [`PrivateModeScanner`] carries between chunks. A
/// real private-mode sequence is a handful of bytes — `ESC[?1000;1002;1003;1006h`,
/// the longest shape any of these agents emits, is 24 — so 64 leaves room for
/// roughly a dozen parameters while keeping the ceiling nowhere near a PTY read.
/// It exists because the carry is driven by the child: without it, a stream that
/// opens `ESC[?` and never terminates it would grow the buffer for as long as
/// the agent kept talking.
pub(crate) const CARRY_MAX: usize = 64;

/// Finds DEC private-mode directives — `ESC [ ? <params> h` (set) and
/// `ESC [ ? <params> l` (reset) — in a stream delivered in arbitrary chunks.
///
/// A PTY read boundary falls wherever the kernel had bytes ready, so
/// `ESC[?100` + `2h` is an ordinary pair of reads; the trailing bytes that could
/// still open a sequence are carried into the next chunk. Parameters are parsed
/// as whole numbers, so `ESC[?11000h` never names 1000, and every parameter of
/// a combined `ESC[?1000;1006h` is reported, in order.
#[derive(Debug, Default, Clone)]
pub(crate) struct PrivateModeScanner {
    /// The trailing bytes of the previous chunk that could still be the prefix
    /// of a private-mode sequence — never more than [`CARRY_MAX`].
    carry: Vec<u8>,
}

impl PrivateModeScanner {
    /// The bytes held over for the next chunk.
    #[cfg(test)]
    pub(crate) fn carry(&self) -> &[u8] {
        &self.carry
    }

    /// Scan this scanner's carry-over followed by `data`, calling
    /// `on_mode(mode, set)` for every mode a directive names, **in byte order**.
    pub(crate) fn scan(&mut self, data: &[u8], mut on_mode: impl FnMut(u32, bool)) {
        const ESC: u8 = 0x1b;

        let carry = std::mem::take(&mut self.carry);
        let joined: Vec<u8>;
        let buf: &[u8] = if carry.is_empty() {
            // The overwhelmingly common case, and the hot path: no copy at all.
            data
        } else {
            let mut v = Vec::with_capacity(carry.len() + data.len());
            v.extend_from_slice(&carry);
            v.extend_from_slice(data);
            joined = v;
            &joined
        };

        // Where a sequence that is still open when the buffer runs out began.
        // Only that suffix is worth carrying — everything before it is decided.
        let mut partial_from: Option<usize> = None;
        let mut i = 0usize;

        while i < buf.len() {
            if buf[i] != ESC {
                i += 1;
                continue;
            }
            let seq_start = i;
            // `ESC [ ?` — the private-mode introducer. Running out
            // mid-introducer is a partial, not a miss; anything else here is
            // some other escape sequence, so resume scanning after the ESC.
            if i + 1 >= buf.len() {
                partial_from = Some(seq_start);
                break;
            }
            if buf[i + 1] != b'[' {
                i += 1;
                continue;
            }
            if i + 2 >= buf.len() {
                partial_from = Some(seq_start);
                break;
            }
            if buf[i + 2] != b'?' {
                i += 1;
                continue;
            }

            // Parameter list: `;`-separated decimal numbers, then a final byte.
            let params_start = i + 3;
            let mut j = params_start;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j >= buf.len() {
                partial_from = Some(seq_start);
                break;
            }
            if buf[j] == ESC {
                // A fresh introducer aborted this one (malformed output).
                // Resync ON it rather than consuming it, so a run of truncated
                // sequences does not swallow every other one.
                i = j;
                continue;
            }

            let set = match buf[j] {
                b'h' => Some(true),
                b'l' => Some(false),
                // Some other final byte: a request or report (`ESC[?1000$p`),
                // not a directive. Skip past it.
                _ => None,
            };
            if let Some(set) = set {
                for param in buf[params_start..j].split(|&b| b == b';') {
                    // All-digit by construction; an empty or absurdly long
                    // parameter simply names no mode.
                    let mode = std::str::from_utf8(param)
                        .ok()
                        .and_then(|text| text.parse::<u32>().ok());
                    if let Some(mode) = mode {
                        on_mode(mode, set);
                    }
                }
            }
            i = j + 1;
        }

        if let Some(from) = partial_from {
            let tail = &buf[from..];
            if tail.len() <= CARRY_MAX {
                self.carry = tail.to_vec();
            }
            // Over the cap the carry is simply dropped. No real private-mode
            // sequence is anywhere near this long, so what is open is malformed
            // or hostile, and refusing it costs at most one missed directive —
            // never a mode invented from bytes that were never seen whole.
        }
    }
}

/// Which mouse-reporting protocol the child has selected, if any.
///
/// These four DEC private modes are **one mutually exclusive field**, not four
/// independent switches — exactly as `vt100` 0.16.2 models them
/// (`Screen::set_mouse_mode` assigns `mouse_protocol_mode`, it does not or-in a
/// bit). Setting 1003 after 1000 leaves the child reporting any-motion and
/// nothing else, and a DECRST clears reporting only when it names the mode
/// currently in force (`Screen::clear_mouse_mode`).
///
/// 1004 is deliberately absent, and is not a protocol at all: it is focus
/// reporting, which codex sets on its own (PRD #611). Treating it as mouse would
/// break the exact case that PRD exists for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseProtocol {
    /// No reporting: nothing the child asked for will be sent to it.
    #[default]
    None,
    /// `9` — X10 compatibility mode, press only.
    Press,
    /// `1000` — normal tracking (VT200): press and release.
    PressRelease,
    /// `1002` — button-event tracking: press, release and drag.
    ButtonMotion,
    /// `1003` — any-event tracking: every motion, button or not.
    AnyMotion,
}

impl MouseProtocol {
    fn for_mode(mode: u32) -> Option<Self> {
        match mode {
            9 => Some(Self::Press),
            1000 => Some(Self::PressRelease),
            1002 => Some(Self::ButtonMotion),
            1003 => Some(Self::AnyMotion),
            _ => None,
        }
    }

    fn mode(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Press => Some(9),
            Self::PressRelease => Some(1000),
            Self::ButtonMotion => Some(1002),
            Self::AnyMotion => Some(1003),
        }
    }
}

/// How the child expects a mouse report to be **encoded** — a separate field
/// from [`MouseProtocol`], and one that enables no reporting on its own.
///
/// This is the second half of `vt100`'s model (`set_mouse_encoding` /
/// `clear_mouse_encoding`), and the half a scanner that treats 1006 as "mouse is
/// on" gets wrong: `ESC[?1006h` by itself asks for SGR-encoded reports of a
/// protocol nobody has selected, which means no reports at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseEncoding {
    /// The original X10 encoding: `ESC[M` plus three offset-by-32 bytes.
    #[default]
    Default,
    /// `1005` — UTF-8 extended coordinates.
    Utf8,
    /// `1006` — SGR extended: `ESC[<b;col;rowM`.
    Sgr,
}

impl MouseEncoding {
    fn for_mode(mode: u32) -> Option<Self> {
        match mode {
            1005 => Some(Self::Utf8),
            1006 => Some(Self::Sgr),
            _ => None,
        }
    }

    fn mode(self) -> Option<u32> {
        match self {
            Self::Default => None,
            Self::Utf8 => Some(1005),
            Self::Sgr => Some(1006),
        }
    }
}

/// The mouse-reporting state a child has requested: one protocol and one
/// encoding, each a single field.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseModes {
    pub(crate) protocol: MouseProtocol,
    pub(crate) encoding: MouseEncoding,
}

impl MouseModes {
    /// Apply one directive. Returns whether `mode` names a mouse protocol or
    /// encoding at all, whether or not the state changed.
    pub(crate) fn apply(&mut self, mode: u32, set: bool) -> bool {
        if let Some(protocol) = MouseProtocol::for_mode(mode) {
            // One field, overwritten by a SET. A RESET clears reporting only
            // when it names the protocol actually in force — an app withdrawing
            // 1000 after it moved on to 1002 has withdrawn nothing.
            if set {
                self.protocol = protocol;
            } else if self.protocol == protocol {
                self.protocol = MouseProtocol::None;
            }
            true
        } else if let Some(encoding) = MouseEncoding::for_mode(mode) {
            // Same shape, separate field: selecting an encoding turns no
            // reporting on, and withdrawing one that is not in force turns none
            // off.
            if set {
                self.encoding = encoding;
            } else if self.encoding == encoding {
                self.encoding = MouseEncoding::Default;
            }
            true
        } else {
            false
        }
    }
}

/// Issue #1537 — the terminal modes a replay must re-establish because the
/// bytes that set them may no longer be in the replay.
///
/// A full-screen agent (claude with `"tui": "fullscreen"`, measured) sends
/// `ESC[?1049h` once at start-up and never again — not on SIGWINCH and not on
/// focus-in — and repaints in place from then on. The daemon's output ring
/// drops everything on a resize and its oldest bytes past its cap, so a ring
/// rebuilt into a fresh `vt100` parser left that parser on the normal screen
/// while the agent was still on the alternate one. Everything that then reads
/// the parser — the cannot-scroll notice first of all — described a screen the
/// agent was not on.
///
/// Only the modes `vt100` keeps as screen state and the deck acts on are
/// tracked: the alternate screen (`47` and `1049`; `vt100` ignores `1047`) and
/// mouse reporting.
#[derive(Debug, Default, Clone)]
pub(crate) struct ReplayModes {
    scanner: PrivateModeScanner,
    alternate_screen: bool,
    mouse: MouseModes,
}

impl ReplayModes {
    /// Advance the tracked state past `data`.
    pub(crate) fn feed(&mut self, data: &[u8]) {
        let Self {
            scanner,
            alternate_screen,
            mouse,
        } = self;
        scanner.scan(data, |mode, set| match mode {
            47 | 1049 => *alternate_screen = set,
            _ => {
                mouse.apply(mode, set);
            }
        });
    }

    /// The bytes that put a fresh parser into the tracked state. Empty when
    /// every tracked mode is at its default, so a replay of a plain stream is
    /// byte-for-byte what it was.
    pub(crate) fn preamble(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.alternate_screen {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        for mode in [self.mouse.protocol.mode(), self.mouse.encoding.mode()]
            .into_iter()
            .flatten()
        {
            out.extend_from_slice(format!("\x1b[?{mode}h").as_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes_after(chunks: &[&[u8]]) -> ReplayModes {
        let mut modes = ReplayModes::default();
        for chunk in chunks {
            modes.feed(chunk);
        }
        modes
    }

    /// Feed `preamble` to a fresh `vt100` parser — the thing a replay is for.
    fn parsed(preamble: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(preamble);
        parser
    }

    #[test]
    fn a_plain_stream_needs_no_preamble() {
        assert!(
            modes_after(&[b"hello\r\nworld\x1b[2J\x1b[H"])
                .preamble()
                .is_empty()
        );
    }

    #[test]
    fn the_alternate_screen_and_mouse_modes_a_fullscreen_agent_sets_are_restored() {
        let modes = modes_after(&[b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h"]);
        let parser = parsed(&modes.preamble());
        assert!(parser.screen().alternate_screen());
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::AnyMotion
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );
    }

    #[test]
    fn leaving_the_alternate_screen_and_withdrawing_the_mouse_clears_them() {
        let modes = modes_after(&[
            b"\x1b[?1049h\x1b[?1000;1006h",
            b"\x1b[?1049l\x1b[?1000l\x1b[?1006l",
        ]);
        assert!(modes.preamble().is_empty());
    }

    #[test]
    fn mode_47_counts_as_the_alternate_screen_and_1047_does_not() {
        assert!(
            parsed(&modes_after(&[b"\x1b[?47h"]).preamble())
                .screen()
                .alternate_screen()
        );
        assert!(modes_after(&[b"\x1b[?1047h"]).preamble().is_empty());
    }

    #[test]
    fn a_directive_split_across_chunks_is_still_seen() {
        let modes = modes_after(&[b"text\x1b[?10", b"49h more"]);
        assert!(parsed(&modes.preamble()).screen().alternate_screen());
    }

    #[test]
    fn withdrawing_a_mouse_protocol_that_is_not_in_force_withdraws_nothing() {
        let modes = modes_after(&[b"\x1b[?1000h\x1b[?1002h\x1b[?1000l\x1b[?1006h"]);
        let parser = parsed(&modes.preamble());
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::ButtonMotion
        );
    }
}
