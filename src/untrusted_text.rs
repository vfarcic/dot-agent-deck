//! Sanitizers for producer-supplied text that ends up rendered into the live
//! terminal.
//!
//! Several strings on this project's wire are written by a peer we do not
//! control — hook events arrive on a socket any agent on the deck can post to,
//! and `list_agents` records are echoed by a daemon that may be older or
//! malformed. Every one of those strings is eventually drawn into a ratatui
//! cell or written to the pre-alt-screen terminal, so a raw ESC, a NUL or a
//! Unicode bidi override in one of them can repaint, reorder or hide text the
//! user is relying on to make a decision.
//!
//! Two policies live here, and which one is right depends on where the value
//! lands. [`strip_control_and_bidi`] **strips**, because its values land in
//! width-constrained regions (a dashboard card title gets whatever columns the
//! status badge leaves it) and `\x1b` expanding to four visible characters
//! would spend that budget on the attacker's behalf.
//! [`escape_control_and_bidi`] **escapes**, because a diagnostic line wants to
//! *show* that the producer sent something peculiar rather than quietly hide
//! it, and has a whole scrollback line to spend saying so — see
//! `keybindings::sanitize_for_terminal`, which does the same for a warning
//! printed once to stderr, and `remote_doctor::render`, which routes every
//! producer-controlled field of its report through the escaping form (PRD
//! #345 audit). Pick stripping for a value that has to stay readable inside a
//! fixed box, escaping for one whose whole purpose is to be inspected.
//!
//! This is now the single implementation of the control+bidi filter.
//! [`crate::build_version_handshake`] carried a byte-identical copy until issue
//! #670 and delegates here instead. [`crate::daemon_client`] still has its own
//! `strip_control_chars`, which drops control characters but NOT bidi — that is
//! a different seam (the `list_agents` wire boundary, on records the daemon has
//! already validated) with its own audit, and folding it in here is deliberately
//! not part of #670. Reach for this module rather than writing a third copy: it
//! is the divergence between those two that let the bidi half be present on one
//! untrusted path and absent on another.
//!
//! Issue #833 moved ONE field off that older filter: `AgentRecord.display_name`
//! at the same `list_agents` boundary now goes through
//! [`sanitize_display_name`] here, because it is the card title
//! `ui::render_card_grid` prefers and the daemon-side gate that was supposed to
//! keep it clean admitted bidi. The live-snapshot strings beside it —
//! `last_user_prompt`, `first_prompts`, `active_tool` — still use
//! `daemon_client`'s control-only filter, so that seam is now MIXED rather than
//! uniformly control-only. Their live counterparts arriving on the hook socket
//! are the ones this module covers: `tool_name` / `tool_detail` through
//! [`sanitize_tool_text`], and the hook's `display_name` metadata through
//! [`sanitize_display_name`]. `user_prompt` at that ingest is scrubbed on
//! neither route and is not part of #833.

use crate::agent_pty::DISPLAY_NAME_MAX_LEN;
use crate::prompt_delivery::truncate_on_char_boundary;

/// Returns `true` for Unicode bidirectional formatting / override codepoints.
///
/// [`char::is_control`] does **not** catch these — they are general category
/// `Cf`, not `Cc` — but a terminal honours them, so a `U+202E`
/// (RIGHT-TO-LEFT OVERRIDE) planted in an untrusted string visually reverses
/// the characters after it. That is enough to make a name read as something it
/// is not, or to swallow the text that follows it on the same line.
pub fn is_bidi_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}'   // LRE, RLE, PDF, LRO, RLO
            | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
            | '\u{200E}'              // LRM
            | '\u{200F}'              // RLM
            | '\u{061C}'              // ALM
    )
}

/// Drop every character from `s` that could perturb or spoof the terminal it is
/// rendered into:
///
/// - C0 control bytes (`< 0x20`) and `0x7F` (DEL),
/// - C1 controls (`0x80..=0x9F`, including U+0085 NEL) — both covered by
///   [`char::is_control`],
/// - the bidi formatting / override codepoints [`is_bidi_format_char`] names.
///
/// `keep_newlines` retains `\n` for callers whose own output is multi-line and
/// whose line structure is theirs rather than the producer's. Pass `false` for
/// any value that has to stay on one line — an embedded newline in a card title
/// or a prompt name is an extra line the producer did not get to ask for.
pub fn strip_control_and_bidi(s: &str, keep_newlines: bool) -> String {
    s.chars()
        .filter(|&c| {
            if keep_newlines && c == '\n' {
                return true;
            }
            !(c.is_control() || is_bidi_format_char(c))
        })
        .collect()
}

/// Escape, rather than drop, every character [`strip_control_and_bidi`] would
/// remove: C0/C1 controls (ESC, NUL, CR, LF, DEL and friends) become their
/// `\n` / `\u{1b}` source form, and the bidi formatting / override
/// codepoints become `\u{202e}`-style escapes. Everything else — accents,
/// CJK, emoji, punctuation — passes through byte for byte.
///
/// Use this on producer-controlled text written to a **diagnostic**, where the
/// reader is trying to work out what a peer actually sent. Silently stripping
/// there would hide the very evidence the line exists to carry, and the
/// diagnostic's own conclusions are what an attacker wants to repaint: PRD
/// #345's `remote doctor` quotes ssh's stderr, the remote's registry entry and
/// the listen specs `ssh -G` resolved, every one of which a hostile remote or
/// a planted `~/.ssh/config` can fill with CSI/OSC sequences that clear the
/// screen, retitle the terminal or forge a hyperlink.
///
/// The result contains no control characters, so applying this twice is a
/// no-op on the second pass — callers may escape at the boundary where the
/// value enters an error type *and* again at the render seam without producing
/// `\\u{1b}`.
pub fn escape_control_and_bidi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else if is_bidi_format_char(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// Sanitize a producer-supplied display name into something safe to store and
/// render, or `None` when nothing usable survives.
///
/// Strips control and bidi characters, trims surrounding whitespace, then
/// clamps to [`DISPLAY_NAME_MAX_LEN`] bytes on a character boundary via
/// [`clamp_including_marker`] — which builds on the same truncator
/// `ui::render_session_card` already applies to the equally producer-controlled
/// `session_id`, so a clamped name is marked with the same trailing `…` the
/// user sees on a shortened id. `None` means "no usable name": the caller
/// should leave whatever name it already had in place rather than store an
/// empty title.
///
/// The byte ceiling is deliberately the daemon's own
/// [`DISPLAY_NAME_MAX_LEN`], so a name that reaches a card through the hook
/// socket cannot be longer than one that reaches it through
/// `agent_pty::is_valid_display_name` on the attach socket. That is now true of
/// the WHOLE returned string rather than of its body: the ceiling used to be
/// applied before the `…` was appended, so this could return
/// `DISPLAY_NAME_MAX_LEN + 3` bytes — a name the daemon's gate would refuse if
/// it ever came back the other way (Greptile, PR #902). See
/// [`clamp_including_marker`]. The two differ in
/// what they do about a bad value — the daemon **rejects** the whole name,
/// this **repairs** it — because the daemon is validating a request a user just
/// typed and can retype, while this is scrubbing a field on an event whose
/// other contents we still want to apply.
///
/// Note the clamp counts BYTES, not display columns. That matches every other
/// length bound in this project and is not the render budget: the title's own
/// fit is decided later by `ui::truncate_styled_segments`, whose char-vs-column
/// accounting is issue #357.
pub fn sanitize_display_name(raw: &str) -> Option<String> {
    let stripped = strip_control_and_bidi(raw, false);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(clamp_including_marker(trimmed, DISPLAY_NAME_MAX_LEN))
}

/// Clamp `s` so the RESULT — the `…` cut marker included — is at most `max`
/// bytes, snapping back to a character boundary.
///
/// [`truncate_on_char_boundary`] keeps up to `max` bytes and THEN appends the
/// three-byte marker, so its result can be `max + 3`. That is right for its own
/// callers, whose `max` is a render budget: `ui::render_session_card` is fitting
/// a session id into a title, and three bytes of marker are part of what it
/// draws. It is wrong for both callers here, whose `max` is a byte ceiling some
/// OTHER component enforces — [`DISPLAY_NAME_MAX_LEN`] is the length
/// `agent_pty::is_valid_display_name` rejects past, and
/// [`MAX_TOOL_TEXT_BYTES`] is the length `daemon_client` clamps the same two
/// tool strings to on their other route, strictly and with no marker. A
/// sanitizer that emitted `max + 3` would produce a display name the daemon's
/// own gate refuses, which is exactly the round trip `sanitize_display_name`'s
/// doc claims cannot happen (Greptile, PR #902).
///
/// A value already within `max` passes through untouched, marker and all — the
/// reservation is made only when a cut is actually needed, so a name that
/// exactly fills the ceiling is not marked as truncated for the sake of room it
/// does not use.
fn clamp_including_marker(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    truncate_on_char_boundary(s, max.saturating_sub(ELLIPSIS_LEN))
}

/// Byte length of the `…` both clamps here reserve room for.
const ELLIPSIS_LEN: usize = '…'.len_utf8();

/// Byte ceiling applied to a producer-supplied tool name or tool detail at
/// hook ingest ([`sanitize_tool_text`]).
///
/// Deliberately the same 64 KiB `crate::daemon_client` clamps the SAME two
/// strings to when they arrive over the `list_agents` echo instead of over the
/// hook socket, and — since Greptile's review of PR #902 — the same INCLUSIVE
/// of the `…` cut marker, so neither route can return a longer string than the
/// other. The two routes carry one field between them, so a ceiling that
/// differed by route would mean the card's tool line had two different maximum
/// lengths depending on whether the TUI had reconnected — and issue #833 is
/// precisely a defect of one field's two routes disagreeing.
pub const MAX_TOOL_TEXT_BYTES: usize = 65536;

/// Sanitize a producer-supplied tool name or tool detail into something safe to
/// store and render.
///
/// Strips control and bidi characters, then clamps to [`MAX_TOOL_TEXT_BYTES`]
/// on a character boundary via [`truncate_on_char_boundary`], marking the cut
/// with a trailing `…` exactly as [`sanitize_display_name`] does.
///
/// Unlike [`sanitize_display_name`] this returns a `String` rather than an
/// `Option`, and does NOT trim: an empty result is a meaningful value here (the
/// tool simply reported no detail) where an empty card title is not, and the
/// leading/trailing whitespace of a tool detail is the producer's own
/// formatting of a command line rather than padding around a name.
pub fn sanitize_tool_text(raw: &str) -> String {
    clamp_including_marker(&strip_control_and_bidi(raw, false), MAX_TOOL_TEXT_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_control_and_bidi_removes_escapes_del_c1_and_overrides() {
        // One string carrying every class at once: ESC, NUL, CR/LF, DEL, a C1
        // control that only `char::is_control` catches, and an RLO.
        let dirty = "ze\x1b[31mta\0-li\u{202e}ve\x7f-\u{0085}77\r\n";
        assert_eq!(strip_control_and_bidi(dirty, false), "ze[31mta-live-77");

        // `keep_newlines` is the ONLY exemption, and it is opt-in.
        assert_eq!(strip_control_and_bidi("a\nb", false), "ab");
        assert_eq!(strip_control_and_bidi("a\n\u{202e}b\x1b", true), "a\nb");

        // Text outside the ASCII range is untouched — names are UTF-8 and
        // legitimately contain accents, CJK and emoji.
        assert_eq!(
            strip_control_and_bidi("café-日本-🎉", false),
            "café-日本-🎉"
        );
    }

    #[test]
    fn strip_control_and_bidi_covers_every_bidi_codepoint() {
        // Enumerated rather than spot-checked: a range typo in
        // `is_bidi_format_char` would otherwise leave one override live, and one
        // is all a spoof needs.
        for c in ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}']
            .into_iter()
            .chain(['\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'])
            .chain(['\u{200E}', '\u{200F}', '\u{061C}'])
        {
            assert!(
                is_bidi_format_char(c),
                "U+{:04X} must be recognised",
                c as u32
            );
            let input = format!("a{c}b");
            assert_eq!(
                strip_control_and_bidi(&input, false),
                "ab",
                "U+{:04X} survived the filter",
                c as u32
            );
        }
    }

    #[test]
    fn escape_control_and_bidi_shows_what_was_sent_instead_of_hiding_it() {
        // The same fixture the stripping test uses, so the two policies are
        // directly comparable: every class survives as visible evidence.
        let dirty = "ze\x1b[31mta\0-li\u{202e}ve\x7f-\u{0085}77\r\n";
        let escaped = escape_control_and_bidi(dirty);
        assert_eq!(
            escaped,
            "ze\\u{1b}[31mta\\u{0}-li\\u{202e}ve\\u{7f}-\\u{85}77\\r\\n"
        );
        // Nothing a terminal acts on may survive — that is the whole point.
        assert!(
            !escaped
                .chars()
                .any(|c| c.is_control() || is_bidi_format_char(c)),
            "escaped output still carries a live control/bidi character: {escaped:?}"
        );

        // Ordinary text is untouched, so a report stays readable.
        assert_eq!(escape_control_and_bidi("café-日本-🎉"), "café-日本-🎉");
        assert_eq!(
            escape_control_and_bidi("port 1080 is bound"),
            "port 1080 is bound"
        );
    }

    #[test]
    fn escape_control_and_bidi_is_idempotent() {
        // Callers escape at the error boundary AND again at the render seam;
        // a second pass must not turn `\u{1b}` into `\\u{1b}`.
        for raw in ["\x1b]0;pwn\x07", "a\u{202e}b", "plain", "back\\slash"] {
            let once = escape_control_and_bidi(raw);
            assert_eq!(
                escape_control_and_bidi(&once),
                once,
                "escaping {raw:?} twice changed the result"
            );
        }
    }

    #[test]
    fn sanitize_display_name_scrubs_trims_and_drops_empties() {
        assert_eq!(
            sanitize_display_name("fix-auth-bug"),
            Some("fix-auth-bug".to_string())
        );
        assert_eq!(
            sanitize_display_name("  \x1b]0;pwn\x07 dispatch-670 \n"),
            Some("]0;pwn dispatch-670".to_string())
        );

        // Nothing usable survives → `None`, so the caller keeps the name it had
        // instead of blanking a card title. This is the case the old
        // `.filter(|n| !n.is_empty())` guard covered for the literal empty
        // string only.
        assert_eq!(sanitize_display_name(""), None);
        assert_eq!(sanitize_display_name("   "), None);
        assert_eq!(sanitize_display_name("\x1b\u{202e}\0\x7f"), None);
    }

    #[test]
    fn sanitize_display_name_clamps_on_a_char_boundary() {
        // ASCII: exactly at the ceiling passes through untouched, one over is
        // cut and marked.
        let at_cap = "a".repeat(DISPLAY_NAME_MAX_LEN);
        assert_eq!(sanitize_display_name(&at_cap), Some(at_cap.clone()));
        let over = "a".repeat(DISPLAY_NAME_MAX_LEN + 1);
        let clamped = sanitize_display_name(&over).expect("a long name is repaired, not dropped");
        // The marker is INSIDE the ceiling (Greptile, PR #902): the whole
        // returned string must be something `agent_pty::is_valid_display_name`
        // would still accept, so the body gives up three bytes to it rather
        // than the result running three bytes over.
        assert_eq!(
            clamped,
            format!("{}…", "a".repeat(DISPLAY_NAME_MAX_LEN - '…'.len_utf8()))
        );
        assert!(
            clamped.len() <= DISPLAY_NAME_MAX_LEN,
            "the marker must fit inside the ceiling, got {} bytes",
            clamped.len()
        );

        // Multi-byte: the cut must snap DOWN to a boundary rather than split a
        // character. Swept across widths and offsets because the surviving byte
        // count depends on where the ceiling lands inside the fixture, and a
        // single fixture that happens to align proves nothing.
        for filler in ['α', 'あ', '𝄞', '😀'] {
            for pad in 0..4usize {
                let raw = "x".repeat(pad) + &filler.to_string().repeat(DISPLAY_NAME_MAX_LEN);
                let out = sanitize_display_name(&raw).expect("non-empty input yields a name");
                assert!(
                    out.len() <= DISPLAY_NAME_MAX_LEN,
                    "clamp overshot the ceiling MARKER INCLUDED: {} bytes for \
                     filler {filler:?} pad {pad}",
                    out.len()
                );
                let body = out.strip_suffix('…').expect("an over-long name is marked");
                assert!(
                    body.ends_with(filler),
                    "the cut split a character for filler {filler:?} pad {pad}: {body:?}"
                );
            }
        }
    }

    #[test]
    fn sanitize_tool_text_strips_and_clamps_without_trimming() {
        // Issue #833. Same strip policy as a display name — the tool line is a
        // width-constrained region on the same card — but no trim and no
        // `Option`: an empty detail is a meaningful value where an empty card
        // title is not, and the surrounding whitespace of a command line is
        // the producer's own formatting rather than padding around a name.
        assert_eq!(sanitize_tool_text("Bash"), "Bash");
        assert_eq!(
            sanitize_tool_text("src/\u{202e}gnidaer\u{7f}\r\n rm -rf /"),
            "src/gnidaer rm -rf /"
        );
        assert_eq!(sanitize_tool_text(""), "");
        assert_eq!(sanitize_tool_text("\u{1b}\u{202e}\0\u{7f}"), "");
        // Whitespace is the producer's, and is kept.
        assert_eq!(sanitize_tool_text("  git status  "), "  git status  ");

        // Clamped on a character boundary and marked, like a display name.
        let over = "\u{3bc}".repeat(MAX_TOOL_TEXT_BYTES);
        let out = sanitize_tool_text(&over);
        // Marker inside the ceiling, matching the strict, marker-less clamp
        // `daemon_client` applies to these same two strings on their other
        // route (Greptile, PR #902).
        assert!(
            out.len() <= MAX_TOOL_TEXT_BYTES,
            "clamp overshot the ceiling MARKER INCLUDED: {} bytes",
            out.len()
        );
        let body = out.strip_suffix('…').expect("an over-long value is marked");
        assert!(
            body.len() <= MAX_TOOL_TEXT_BYTES,
            "clamp overshot the ceiling: {} bytes",
            body.len()
        );
        // A value that exactly fills the ceiling is NOT marked: the three bytes
        // are reserved only when a cut is actually needed.
        let at_cap = "a".repeat(MAX_TOOL_TEXT_BYTES);
        assert_eq!(sanitize_tool_text(&at_cap), at_cap);
        assert!(
            body.ends_with('\u{3bc}'),
            "the cut split a character: {body:?}"
        );

        // Stripping happens BEFORE the clamp, so a value padded past the
        // ceiling with control bytes is a real value once scrubbed rather than
        // 64 KiB of padding cut down and then scrubbed to nothing.
        let padded = format!("{}{}", "\u{1b}".repeat(MAX_TOOL_TEXT_BYTES + 10), "Bash");
        assert_eq!(sanitize_tool_text(&padded), "Bash");
    }

    #[test]
    fn sanitize_display_name_clamps_after_stripping_not_before() {
        // A name padded to well over the ceiling with control bytes is a REAL
        // name once scrubbed. Clamping first would cut it to 128 bytes of
        // padding and then scrub that to nothing, losing a name that fits.
        let padded = format!("{}{}", "\x1b".repeat(400), "nightly-sweep");
        assert_eq!(
            sanitize_display_name(&padded),
            Some("nightly-sweep".to_string())
        );
    }
}
