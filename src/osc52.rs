//! Issue #1705 — an agent's OSC 52 set-clipboard request, read write-only.
//!
//! Agents copy text by asking their terminal to set the clipboard:
//! `ESC ] 52 ; <targets> ; <base64> BEL` (or `ESC \`). In a TUI pane that
//! terminal is the deck's pane parser, so [`crate::hyperlink::Osc8Filter`]
//! lifts each request out of the agent's stream and this decides whether it is
//! a copy the deck passes on to the user's own terminal.
//!
//! WRITE ONLY. A `?` payload asks the terminal to send the clipboard BACK to
//! the agent, and it is never answered or passed on: anything an agent's
//! output can make it print could otherwise read whatever the user last copied.
//!
//! The desktop app applies the same rules to the same request in its own
//! terminal (`desktop/src/lib/osc52.ts`), so both clients copy the same things.

/// The largest copy passed on, in bytes of text. Dropped rather than
/// truncated above it: a copy that silently lost its tail pastes as if whole.
pub const OSC52_MAX_TEXT_BYTES: usize = 1024 * 1024;

/// The text of a set-clipboard request, given everything after `52;`, or
/// `None` — meaning "leave the clipboard as it is" — for anything that is not
/// a well-formed copy: a target letter OSC 52 does not define (`c p q s 0-7`,
/// or none, which is the default), a read request, an empty payload (which
/// would clear the user's clipboard), a payload that is not base64 or whose
/// bytes are not UTF-8, or text over [`OSC52_MAX_TEXT_BYTES`].
pub fn osc52_copy_text(request: &[u8]) -> Option<String> {
    let separator = request.iter().position(|&b| b == b';')?;
    let (targets, payload) = (&request[..separator], &request[separator + 1..]);
    if !targets.iter().all(|b| b"cpqs01234567".contains(b)) {
        return None;
    }
    if payload.is_empty() || payload == b"?" {
        return None;
    }
    if payload.len() > OSC52_MAX_TEXT_BYTES.div_ceil(3) * 4 {
        return None;
    }
    let bytes = base64_decode(payload)?;
    if bytes.is_empty() || bytes.len() > OSC52_MAX_TEXT_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Standard-alphabet base64, padded or not, as `atob` accepts it.
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let trimmed = match input {
        [rest @ .., b'=', b'='] if input.len().is_multiple_of(4) => rest,
        [rest @ .., b'='] if input.len().is_multiple_of(4) => rest,
        _ => input,
    };
    if trimmed.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len() / 4 * 3 + 2);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &byte in trimmed {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(targets: &str, text: &str) -> Vec<u8> {
        let mut out = format!("{targets};").into_bytes();
        out.extend_from_slice(crate::ui::base64_encode(text.as_bytes()).as_bytes());
        out
    }

    #[test]
    fn decodes_the_copy_an_agent_sends() {
        assert_eq!(
            osc52_copy_text(&request("c", "hello")).as_deref(),
            Some("hello")
        );
        assert_eq!(
            osc52_copy_text(&request("c", "naïve — ✓\nnext")).as_deref(),
            Some("naïve — ✓\nnext")
        );
    }

    #[test]
    fn accepts_every_defined_target_and_the_empty_default() {
        for targets in ["", "c", "p", "q", "s", "0", "7", "cp", "s0"] {
            assert_eq!(
                osc52_copy_text(&request(targets, "x")).as_deref(),
                Some("x"),
                "{targets}"
            );
        }
    }

    #[test]
    fn accepts_base64_without_its_padding() {
        assert_eq!(osc52_copy_text(b"c;aGk").as_deref(), Some("hi"));
        assert_eq!(osc52_copy_text(b"c;aGk=").as_deref(), Some("hi"));
        assert_eq!(osc52_copy_text(b"c;aA==").as_deref(), Some("h"));
    }

    #[test]
    fn never_treats_a_read_request_as_a_copy() {
        assert_eq!(osc52_copy_text(b"c;?"), None);
        assert_eq!(osc52_copy_text(b";?"), None);
    }

    #[test]
    fn refuses_a_malformed_request() {
        assert_eq!(osc52_copy_text(b"c"), None);
        assert_eq!(osc52_copy_text(b"c;"), None);
        assert_eq!(osc52_copy_text(&request("z", "x")), None);
        assert_eq!(osc52_copy_text(b"c;not*base64!"), None);
        assert_eq!(osc52_copy_text(b"c;aGkx="), None); // length 5 after the pad
        assert_eq!(osc52_copy_text(b"c;//4="), None); // 0xff 0xfe: not UTF-8
    }

    #[test]
    fn accepts_text_up_to_the_limit_and_refuses_one_byte_more() {
        let at = "a".repeat(OSC52_MAX_TEXT_BYTES);
        assert_eq!(
            osc52_copy_text(&request("c", &at)).map(|t| t.len()),
            Some(OSC52_MAX_TEXT_BYTES)
        );
        let over = "a".repeat(OSC52_MAX_TEXT_BYTES + 1);
        assert_eq!(osc52_copy_text(&request("c", &over)), None);
    }
}
