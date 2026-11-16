//! Agent status from the OSC 7501 terminal status signal (PRD #211).
//!
//! Some agents write their own status to their terminal as an OSC sequence:
//! `ESC ] 7501 ; state=<s>[:kind=<k>]:app=<app>[:msg=<base64>]` terminated by
//! BEL or ST. Google's Antigravity CLI (`agy`) does when `AGY_CLI_OSC7501=on`
//! is in its environment, and it is the only report `agy` makes of a
//! permission prompt and of an interrupted turn: no hook fires for either.
//!
//! The reader is agent-neutral and switched on per agent by the registry
//! ([`crate::agent_registry::AgentSpec::status_osc7501`]), which names the
//! `app` value a pane's sequences must carry and the environment variable that
//! asks the agent to emit them. Only Antigravity switches it on today.
//!
//! Three pieces, each pure and tested here:
//!
//! * [`Osc7501Scanner`] finds the sequences in the PTY output, carried across
//!   reads. It looks only for `ESC ] 7501 ;`, so a pane whose agent has the
//!   reader off pays nothing, and one with it on pays a byte search for ESC.
//! * [`parse_body`] reads one sequence's fields. Unknown keys are ignored; a
//!   `msg` that is not valid base64 costs the message, never the signal.
//! * [`AuthSettle`] holds back `blocked:kind=auth`. `agy` writes it 10–60 ms
//!   before `idle` at every start, signed in or not, so showing it at once
//!   would flash Needs Input at each launch. It is shown only once it has
//!   stood for [`AUTH_SETTLE`] with no other signal after it.
//!
//! Hook events and these signals both move the card, and the daemon applies
//! both through one ordered ingestion path, so the later report wins.

use std::time::{Duration, Instant};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// The bytes after `ESC ]` that open a status sequence.
const PREFIX: &[u8] = b"7501;";
/// The most of one sequence's body the scanner keeps. The fields that decide
/// the signal come first and are short; a `done` carries the whole final reply
/// as base64, which can be long and is not needed, so a longer body keeps its
/// head and loses its `msg`.
pub const MAX_BODY_BYTES: usize = 4096;
/// The most of a decoded message a card is given, in characters.
pub const MAX_MESSAGE_CHARS: usize = 300;
/// How long `blocked:kind=auth` must stand before it is shown. Measured, the
/// launch transient is followed by `idle` within 60 ms; this leaves the daemon
/// a wide margin on a loaded machine while a real sign-in prompt, which waits
/// on the user, still shows within a couple of seconds.
pub const AUTH_SETTLE: Duration = Duration::from_millis(1500);

/// What a status sequence says the agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscState {
    Working,
    Idle,
    /// The turn's end, carrying its reply. `agy` also writes it after an
    /// interrupt, so it reads as Idle and nothing more.
    Done,
    Error,
    Blocked,
}

impl OscState {
    fn parse(value: &[u8]) -> Option<Self> {
        Some(match value {
            b"working" => Self::Working,
            b"idle" => Self::Idle,
            b"done" => Self::Done,
            b"error" => Self::Error,
            b"blocked" => Self::Blocked,
            _ => return None,
        })
    }
}

/// One status sequence, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscSignal {
    pub state: OscState,
    /// `kind` of a `blocked`: `permission`, `question` or `auth` as measured.
    pub kind: Option<String>,
    pub app: Option<String>,
    /// The decoded `msg`, made safe to show ([`clean_message`]); `None` when
    /// absent, not valid base64, or cut by [`MAX_BODY_BYTES`].
    pub msg: Option<String>,
}

/// What a signal does to a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusUpdate {
    Working,
    Idle,
    /// Error, with the agent's message when it gave one.
    Error(Option<String>),
    /// Needs Input, with what the agent is waiting on when it said.
    NeedsInput(Option<String>),
    /// Needs Input on a permission prompt (`blocked:kind=permission`), with
    /// the agent's question when it gave one: the daemon may offer that
    /// question's menu to be answered ([`crate::question::antigravity_permission`]).
    Permission(Option<String>),
}

impl OscSignal {
    /// Whether this is the start-up sign-in signal [`AuthSettle`] holds back.
    pub fn is_auth(&self) -> bool {
        self.state == OscState::Blocked && self.kind.as_deref() == Some("auth")
    }

    /// The card update this signal maps to.
    pub fn update(&self) -> StatusUpdate {
        match self.state {
            OscState::Working => StatusUpdate::Working,
            OscState::Idle | OscState::Done => StatusUpdate::Idle,
            OscState::Error => StatusUpdate::Error(self.msg.clone()),
            OscState::Blocked if self.kind.as_deref() == Some("permission") => {
                StatusUpdate::Permission(self.msg.clone())
            }
            OscState::Blocked => StatusUpdate::NeedsInput(self.msg.clone()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Ground,
    /// Saw ESC.
    Esc,
    /// Saw `ESC ]` and this many bytes of [`PREFIX`].
    Prefix(usize),
    /// Inside the body.
    Body,
    /// Saw ESC inside the body: `\` ends it (ST), anything else abandons it.
    BodyEsc,
}

/// Finds OSC 7501 status sequences in output delivered in arbitrary chunks,
/// keeping only those whose `app` is the one it was made for.
#[derive(Debug, Clone)]
pub struct Osc7501Scanner {
    app: &'static str,
    phase: Phase,
    body: Vec<u8>,
    truncated: bool,
}

impl Osc7501Scanner {
    pub fn new(app: &'static str) -> Self {
        Self {
            app,
            phase: Phase::Ground,
            body: Vec::new(),
            truncated: false,
        }
    }

    /// Scan `data`, carrying a sequence open at its end into the next call,
    /// and push each complete sequence for this scanner's app onto `out`, in
    /// byte order.
    pub fn scan(&mut self, data: &[u8], out: &mut Vec<OscSignal>) {
        let mut i = 0;
        while i < data.len() {
            if self.phase == Phase::Ground {
                match data[i..].iter().position(|&b| b == ESC) {
                    Some(at) => {
                        i += at + 1;
                        self.phase = Phase::Esc;
                    }
                    None => return,
                }
                continue;
            }
            let byte = data[i];
            i += 1;
            self.step(byte, out);
        }
    }

    fn step(&mut self, byte: u8, out: &mut Vec<OscSignal>) {
        self.phase = match self.phase {
            Phase::Ground => {
                if byte == ESC {
                    Phase::Esc
                } else {
                    Phase::Ground
                }
            }
            Phase::Esc => match byte {
                b']' => Phase::Prefix(0),
                ESC => Phase::Esc,
                _ => Phase::Ground,
            },
            Phase::Prefix(matched) => {
                if byte == PREFIX[matched] {
                    if matched + 1 == PREFIX.len() {
                        self.body.clear();
                        self.truncated = false;
                        Phase::Body
                    } else {
                        Phase::Prefix(matched + 1)
                    }
                } else if byte == ESC {
                    Phase::Esc
                } else {
                    Phase::Ground
                }
            }
            Phase::Body => match byte {
                BEL => {
                    self.finish(out);
                    Phase::Ground
                }
                ESC => Phase::BodyEsc,
                // CAN and SUB cancel a control string; any other C0 control
                // is not part of a well-formed body either.
                0x00..=0x1f => Phase::Ground,
                _ => {
                    if self.body.len() < MAX_BODY_BYTES {
                        self.body.push(byte);
                    } else {
                        self.truncated = true;
                    }
                    Phase::Body
                }
            },
            Phase::BodyEsc => {
                if byte == b'\\' {
                    self.finish(out);
                    Phase::Ground
                } else {
                    // The ESC began another sequence: read this byte as its
                    // second.
                    self.phase = Phase::Esc;
                    return self.step(byte, out);
                }
            }
        };
    }

    fn finish(&mut self, out: &mut Vec<OscSignal>) {
        let body = std::mem::take(&mut self.body);
        if let Some(signal) = parse_body(&body, self.truncated)
            && signal.app.as_deref() == Some(self.app)
        {
            out.push(signal);
        }
        self.truncated = false;
    }
}

/// Read one sequence's body: colon-separated `key=value` fields. `None` when it
/// names no state this reader knows (the `?` enablement probe among them).
/// `truncated` says the body was cut, so its `msg` is not decoded.
pub fn parse_body(body: &[u8], truncated: bool) -> Option<OscSignal> {
    let mut state = None;
    let mut kind = None;
    let mut app = None;
    let mut msg = None;
    for field in body.split(|&b| b == b':') {
        let Some(eq) = field.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&field[..eq], &field[eq + 1..]);
        match key {
            b"state" => state = OscState::parse(value),
            b"kind" => kind = Some(String::from_utf8_lossy(value).into_owned()),
            b"app" => app = Some(String::from_utf8_lossy(value).into_owned()),
            b"msg" if !truncated => {
                msg = decode_base64(value)
                    .map(|bytes| clean_message(&String::from_utf8_lossy(&bytes)))
                    .filter(|text| !text.is_empty());
            }
            _ => {}
        }
    }
    Some(OscSignal {
        state: state?,
        kind,
        app,
        msg,
    })
}

/// Agent text as a card may show it: control characters dropped, whitespace
/// runs folded to one space, at most [`MAX_MESSAGE_CHARS`].
pub fn clean_message(text: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if c.is_control() {
            continue;
        }
        if space {
            if count == MAX_MESSAGE_CHARS {
                break;
            }
            out.push(' ');
            count += 1;
            space = false;
        }
        if count == MAX_MESSAGE_CHARS {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// Standard-alphabet base64, padded or not. `None` for any other byte, or a
/// length no encoding produces.
pub fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    fn value(b: u8) -> Option<u32> {
        Some(match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let trimmed = match input {
        [rest @ .., b'=', b'='] => rest,
        [rest @ .., b'='] => rest,
        all => all,
    };
    if trimmed.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    for chunk in trimmed.chunks(4) {
        let mut acc = 0u32;
        for &b in chunk {
            acc = (acc << 6) | value(b)?;
        }
        match chunk.len() {
            4 => out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]),
            3 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
            2 => out.push((acc >> 4) as u8),
            _ => return None,
        }
    }
    Some(out)
}

/// Holds back one pane's `blocked:kind=auth` until it has stood for
/// [`AUTH_SETTLE`] (see the module docs). Every other signal goes straight
/// through and discards a held one.
#[derive(Debug, Default, Clone)]
pub struct AuthSettle {
    /// The held signal: when it is due, when it was seen, and its message.
    held: Option<(Instant, Instant, Option<String>)>,
}

impl AuthSettle {
    /// Take in `signal`, seen at `now`: the update to apply now, if any.
    pub fn signal(&mut self, signal: &OscSignal, now: Instant) -> Option<StatusUpdate> {
        if signal.is_auth() {
            // A repeat keeps the first one's clock: it has stood since then.
            if self.held.is_none() {
                self.held = Some((now + AUTH_SETTLE, now, signal.msg.clone()));
            }
            return None;
        }
        self.held = None;
        Some(signal.update())
    }

    /// When the held signal is due, if one is held.
    pub fn due_at(&self) -> Option<Instant> {
        self.held.as_ref().map(|(at, _, _)| *at)
    }

    /// The held signal's update, once it is due at `now`, with when it was
    /// seen: the instant it is ordered against hook events by.
    pub fn poll(&mut self, now: Instant) -> Option<(StatusUpdate, Instant)> {
        match &self.held {
            Some((at, _, _)) if *at <= now => {
                let (_, seen, msg) = self.held.take()?;
                Some((StatusUpdate::NeedsInput(msg), seen))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Vec<OscSignal> {
        let mut scanner = Osc7501Scanner::new("antigravity");
        let mut out = Vec::new();
        for chunk in chunks {
            scanner.scan(chunk, &mut out);
        }
        out
    }

    const PERMISSION: &[u8] = b"\x1b]7501;state=blocked:kind=permission:app=antigravity:msg=QXBwcm92ZSBCYXNoOiBybSAtZiBwcm9iZS50eHQ=\x07";

    #[test]
    fn a_well_formed_sequence_is_read_with_its_decoded_message() {
        let mut bytes = b"before ".to_vec();
        bytes.extend_from_slice(PERMISSION);
        bytes.extend_from_slice(b" after\x1b]7501;state=idle:app=antigravity\x07");
        let signals = scan_all(&[&bytes]);
        assert_eq!(
            signals,
            vec![
                OscSignal {
                    state: OscState::Blocked,
                    kind: Some("permission".into()),
                    app: Some("antigravity".into()),
                    msg: Some("Approve Bash: rm -f probe.txt".into()),
                },
                OscSignal {
                    state: OscState::Idle,
                    kind: None,
                    app: Some("antigravity".into()),
                    msg: None,
                },
            ]
        );
        assert_eq!(
            signals[0].update(),
            StatusUpdate::Permission(Some("Approve Bash: rm -f probe.txt".into()))
        );
        assert_eq!(signals[1].update(), StatusUpdate::Idle);
    }

    #[test]
    fn every_captured_sequence_maps_to_its_status() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/antigravity/osc7501-sequences.json"
        ))
        .unwrap();
        for sequence in fixture["sequences"].as_array().unwrap() {
            let bytes = sequence["bytes"].as_str().unwrap().as_bytes();
            let signals = scan_all(&[bytes]);
            let state = sequence["state"].as_str().unwrap();
            if state == "?" {
                assert!(signals.is_empty(), "the probe is not a status");
                continue;
            }
            assert_eq!(signals.len(), 1, "{bytes:?}");
            assert_eq!(
                signals[0].msg.as_deref(),
                sequence["msg_decoded"].as_str(),
                "{bytes:?}"
            );
            assert_eq!(signals[0].kind.as_deref(), sequence["kind"].as_str());
        }
    }

    #[test]
    fn unknown_keys_and_fields_without_a_value_are_ignored() {
        let signals = scan_all(&[
            b"\x1b]7501;id=7:state=error:progress=50:bare:app=antigravity:msg=Qm9vbQ==\x07",
        ]);
        assert_eq!(signals.len(), 1);
        assert_eq!(
            signals[0].update(),
            StatusUpdate::Error(Some("Boom".into()))
        );
    }

    #[test]
    fn bad_base64_costs_the_message_not_the_signal() {
        let signals = scan_all(&[b"\x1b]7501;state=error:app=antigravity:msg=not*base64\x07"]);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].update(), StatusUpdate::Error(None));
        assert_eq!(decode_base64(b"QQ"), Some(b"A".to_vec()));
        assert_eq!(decode_base64(b"QUI="), Some(b"AB".to_vec()));
        assert_eq!(decode_base64(b"Q"), None);
    }

    #[test]
    fn a_sequence_split_across_reads_is_still_read() {
        for split in 1..PERMISSION.len() {
            let signals = scan_all(&[&PERMISSION[..split], &PERMISSION[split..]]);
            assert_eq!(signals.len(), 1, "split at {split}");
            assert_eq!(
                signals[0].msg.as_deref(),
                Some("Approve Bash: rm -f probe.txt")
            );
        }
        let one_byte_reads: Vec<&[u8]> = PERMISSION.chunks(1).collect();
        assert_eq!(scan_all(&one_byte_reads).len(), 1);
    }

    #[test]
    fn st_terminates_a_sequence_like_bel() {
        let signals = scan_all(&[b"\x1b]7501;state=working:app=antigravity\x1b\\"]);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].update(), StatusUpdate::Working);
    }

    #[test]
    fn another_app_another_osc_and_an_abandoned_sequence_report_nothing() {
        assert!(scan_all(&[b"\x1b]7501;state=idle:app=someone-else\x07"]).is_empty());
        assert!(scan_all(&[b"\x1b]7502;state=idle:app=antigravity\x07"]).is_empty());
        assert!(scan_all(&[b"\x1b]0;title state=idle:app=antigravity\x07"]).is_empty());
        assert!(scan_all(&[b"\x1b]7501;state=idle:app=antigravity\x18\x07"]).is_empty());
        // An ESC that abandons one sequence can open the next.
        let signals =
            scan_all(&[b"\x1b]7501;state=idle\x1b]7501;state=working:app=antigravity\x07"]);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].update(), StatusUpdate::Working);
    }

    #[test]
    fn a_long_body_keeps_its_state_and_loses_its_message() {
        let mut bytes = b"\x1b]7501;state=done:app=antigravity:msg=".to_vec();
        bytes.extend(std::iter::repeat_n(b'Q', MAX_BODY_BYTES * 2));
        bytes.push(BEL);
        let signals = scan_all(&[&bytes]);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].update(), StatusUpdate::Idle);
        assert_eq!(signals[0].msg, None);
    }

    #[test]
    fn a_message_is_cleaned_and_bounded() {
        assert_eq!(clean_message("  a\u{1b}[31m\n\tb  "), "a[31m b");
        assert_eq!(
            clean_message(&"x".repeat(MAX_MESSAGE_CHARS * 2))
                .chars()
                .count(),
            MAX_MESSAGE_CHARS
        );
    }

    fn signal(state: OscState, kind: Option<&str>) -> OscSignal {
        OscSignal {
            state,
            kind: kind.map(str::to_string),
            app: Some("antigravity".into()),
            msg: kind.map(|_| "Sign in required".to_string()),
        }
    }

    #[test]
    fn the_launch_auth_transient_never_shows_and_a_standing_one_does() {
        let start = Instant::now();
        let mut settle = AuthSettle::default();
        assert_eq!(
            settle.signal(&signal(OscState::Blocked, Some("auth")), start),
            None
        );
        assert_eq!(settle.poll(start + Duration::from_millis(60)), None);
        assert_eq!(
            settle.signal(
                &signal(OscState::Idle, None),
                start + Duration::from_millis(60)
            ),
            Some(StatusUpdate::Idle)
        );
        assert_eq!(settle.due_at(), None);
        assert_eq!(settle.poll(start + AUTH_SETTLE * 2), None);

        assert_eq!(
            settle.signal(&signal(OscState::Blocked, Some("auth")), start),
            None
        );
        assert_eq!(settle.due_at(), Some(start + AUTH_SETTLE));
        // A repeat does not restart the clock.
        assert_eq!(
            settle.signal(
                &signal(OscState::Blocked, Some("auth")),
                start + Duration::from_millis(500)
            ),
            None
        );
        assert_eq!(
            settle.poll(start + AUTH_SETTLE - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            settle.poll(start + AUTH_SETTLE),
            Some((
                StatusUpdate::NeedsInput(Some("Sign in required".into())),
                start
            )),
            "the held signal is ordered by when it was first seen"
        );
        assert_eq!(settle.due_at(), None);
    }

    #[test]
    fn a_permission_prompt_is_not_held_back() {
        let mut settle = AuthSettle::default();
        assert_eq!(
            settle.signal(
                &signal(OscState::Blocked, Some("permission")),
                Instant::now()
            ),
            Some(StatusUpdate::Permission(Some("Sign in required".into())))
        );
    }
}
