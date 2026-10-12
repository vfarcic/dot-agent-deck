//! OSC 8 hyperlink parsing and row-based URL tracking.
//!
//! Terminal applications emit OSC 8 escape sequences to create clickable
//! hyperlinks.  The vt100 crate does not support OSC 8, so this module
//! intercepts the raw PTY byte stream, strips OSC 8 sequences, and records
//! which screen rows have hyperlink URLs.
//!
//! It also lifts OSC 52 clipboard requests out of the stream (issue #1705):
//! the pane parser would only drop them, and a copy the agent asked for is
//! one the deck can pass on to the user's own terminal (see [`crate::osc52`]).

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Osc8Filter — strips OSC 8 sequences from a byte stream
// ---------------------------------------------------------------------------

/// A segment produced by [`Osc8Filter::process`].
#[derive(Debug, Clone, PartialEq)]
pub enum Osc8Segment {
    /// Plain bytes (no active hyperlink). Feed directly to vt100.
    Text(Vec<u8>),
    /// Bytes that are part of an active hyperlink. Feed to vt100 and
    /// record the cursor row → URL association.
    LinkedText { url: String, bytes: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FilterState {
    Normal,
    Esc,
    OscDigits,
    Osc8Params,
    Osc8Uri,
    Osc8UriEsc,
    OscOther,
    OscOtherEsc,
}

/// State machine that strips OSC 8 hyperlink escape sequences from a byte
/// stream and emits [`Osc8Segment`]s.
///
/// Maintains state across calls so it correctly handles sequences that span
/// multiple read chunks.
#[derive(Debug)]
pub struct Osc8Filter {
    state: FilterState,
    current_url: Option<String>,
    text_buf: Vec<u8>,
    osc_num: Vec<u8>,
    osc_params: Vec<u8>,
    osc_uri: Vec<u8>,
    osc_other_buf: Vec<u8>,
    /// Issue #1705: the OSC being buffered in `osc_other_buf` is an OSC 52
    /// clipboard request (`ESC ] 52 ;`), lifted out on completion rather than
    /// passed to vt100.
    osc_other_is_clipboard: bool,
    /// Issue #1705: the OSC 52 request being buffered has outgrown anything
    /// [`crate::osc52::osc52_copy_text`] would accept, so the rest of it is
    /// discarded as it arrives rather than buffered, and the request dropped.
    osc_other_oversized: bool,
    /// Issue #1705: completed OSC 52 requests, everything after `52;`, in
    /// arrival order, for [`Osc8Filter::take_clipboard_requests`].
    clipboard_requests: Vec<Vec<u8>>,
}

impl Default for Osc8Filter {
    fn default() -> Self {
        Self::new()
    }
}

impl Osc8Filter {
    pub fn new() -> Self {
        Self {
            state: FilterState::Normal,
            current_url: None,
            text_buf: Vec::new(),
            osc_num: Vec::new(),
            osc_params: Vec::new(),
            osc_uri: Vec::new(),
            osc_other_buf: Vec::new(),
            osc_other_is_clipboard: false,
            osc_other_oversized: false,
            clipboard_requests: Vec::new(),
        }
    }

    /// Issue #1705: the OSC 52 clipboard requests completed since the last
    /// call, each as everything after `52;` (targets, `;`, payload), unvalidated
    /// — [`crate::osc52::osc52_copy_text`] decides what each is worth. They are
    /// removed from the stream, so the pane parser never sees them.
    pub fn take_clipboard_requests(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.clipboard_requests)
    }

    pub fn process(&mut self, input: &[u8]) -> Vec<Osc8Segment> {
        let mut segments = Vec::new();

        for &byte in input {
            match self.state {
                FilterState::Normal => {
                    if byte == 0x1b {
                        self.state = FilterState::Esc;
                    } else {
                        self.text_buf.push(byte);
                    }
                }
                FilterState::Esc => self.after_esc(byte),
                FilterState::OscDigits => {
                    if byte.is_ascii_digit() {
                        self.osc_num.push(byte);
                    } else if byte == b';' && self.osc_num == b"8" {
                        self.flush_text(&mut segments);
                        self.osc_params.clear();
                        self.state = FilterState::Osc8Params;
                    } else {
                        self.osc_other_is_clipboard = self.osc_num == b"52" && byte == b';';
                        self.osc_other_buf.clear();
                        self.osc_other_buf.push(0x1b);
                        self.osc_other_buf.push(b']');
                        self.osc_other_buf.extend_from_slice(&self.osc_num);
                        self.osc_other_buf.push(byte);
                        self.osc_num.clear();
                        if byte == 0x07 {
                            self.text_buf
                                .extend_from_slice(&std::mem::take(&mut self.osc_other_buf));
                            self.state = FilterState::Normal;
                        } else {
                            self.state = FilterState::OscOther;
                        }
                    }
                }
                FilterState::Osc8Params => {
                    if byte == b';' {
                        self.osc_uri.clear();
                        self.state = FilterState::Osc8Uri;
                    } else if byte == 0x07 {
                        self.complete_osc8();
                    } else if byte == 0x1b {
                        self.state = FilterState::Osc8UriEsc;
                    } else {
                        self.osc_params.push(byte);
                    }
                }
                FilterState::Osc8Uri => {
                    if byte == 0x07 {
                        self.complete_osc8();
                    } else if byte == 0x1b {
                        self.state = FilterState::Osc8UriEsc;
                    } else {
                        self.osc_uri.push(byte);
                    }
                }
                FilterState::Osc8UriEsc => {
                    if byte == b'\\' {
                        self.complete_osc8();
                    } else {
                        self.osc_uri.push(0x1b);
                        self.osc_uri.push(byte);
                        self.state = FilterState::Osc8Uri;
                    }
                }
                FilterState::OscOther => {
                    if byte == 0x07 {
                        self.complete_osc_other(&[0x07]);
                    } else if byte == 0x1b {
                        self.state = FilterState::OscOtherEsc;
                    } else if self.osc_other_is_clipboard && matches!(byte, 0x18 | 0x1a) {
                        // CAN and SUB cancel a control string. The request is
                        // dropped and the byte goes on to vt100 as in ground.
                        self.abort_clipboard_request();
                        self.text_buf.push(byte);
                    } else {
                        self.push_osc_other(byte);
                    }
                }
                FilterState::OscOtherEsc => {
                    if byte == b'\\' {
                        self.complete_osc_other(&[0x1b, b'\\']);
                    } else if self.osc_other_is_clipboard {
                        // ESC that does not start ST ends an OSC string and
                        // begins a new escape. Drop the clipboard request
                        // rather than absorb what follows, so a request cut
                        // off partway (an agent killed mid-copy) cannot hide
                        // the pane's later output (issue #1705).
                        self.abort_clipboard_request();
                        self.after_esc(byte);
                    } else {
                        self.push_osc_other(0x1b);
                        self.push_osc_other(byte);
                        self.state = FilterState::OscOther;
                    }
                }
            }
        }

        self.flush_text(&mut segments);
        segments
    }

    fn flush_text(&mut self, segments: &mut Vec<Osc8Segment>) {
        if self.text_buf.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.text_buf);
        match &self.current_url {
            Some(url) => segments.push(Osc8Segment::LinkedText {
                url: url.clone(),
                bytes,
            }),
            None => segments.push(Osc8Segment::Text(bytes)),
        }
    }

    /// A non-OSC-8 sequence ended with `terminator`. An OSC 52 request is
    /// lifted out (issue #1705); anything else goes on to vt100 unchanged.
    fn complete_osc_other(&mut self, terminator: &[u8]) {
        let mut sequence = std::mem::take(&mut self.osc_other_buf);
        let oversized = std::mem::take(&mut self.osc_other_oversized);
        if std::mem::take(&mut self.osc_other_is_clipboard) {
            // `ESC ] 5 2 ;` is the first five bytes; the rest is the request.
            if !oversized {
                self.clipboard_requests.push(sequence.split_off(5));
            }
        } else {
            sequence.extend_from_slice(terminator);
            self.text_buf.extend_from_slice(&sequence);
        }
        self.state = FilterState::Normal;
    }

    /// The byte after an `ESC` in ground state: `]` starts an OSC, anything
    /// else is passed on to vt100 with the `ESC` in front of it.
    fn after_esc(&mut self, byte: u8) {
        if byte == b']' {
            self.osc_num.clear();
            self.state = FilterState::OscDigits;
        } else {
            self.text_buf.push(0x1b);
            self.text_buf.push(byte);
            self.state = FilterState::Normal;
        }
    }

    /// Issue #1705: an OSC 52 request ended without its terminator. Nothing of
    /// it is lifted out or passed on.
    fn abort_clipboard_request(&mut self) {
        self.osc_other_buf.clear();
        self.osc_other_is_clipboard = false;
        self.osc_other_oversized = false;
        self.state = FilterState::Normal;
    }

    /// One more byte of a non-OSC-8 sequence. An OSC 52 request is buffered
    /// only up to what [`crate::osc52::osc52_copy_text`] could accept, with as
    /// much again for whitespace inside the payload; past that it is marked
    /// oversized and its remaining bytes are dropped, so an agent cannot grow
    /// this buffer without bound (issue #1705).
    fn push_osc_other(&mut self, byte: u8) {
        if self.osc_other_is_clipboard {
            if self.osc_other_oversized {
                return;
            }
            if self.osc_other_buf.len() >= 2 * crate::osc52::OSC52_MAX_PAYLOAD_BYTES + 16 {
                self.osc_other_oversized = true;
                self.osc_other_buf.truncate(5);
                return;
            }
        }
        self.osc_other_buf.push(byte);
    }

    fn complete_osc8(&mut self) {
        let uri = std::mem::take(&mut self.osc_uri);
        self.osc_params.clear();
        if uri.is_empty() {
            self.current_url = None;
        } else if let Ok(url) = String::from_utf8(uri) {
            self.current_url = Some(url);
        }
        self.state = FilterState::Normal;
    }
}

// ---------------------------------------------------------------------------
// HyperlinkMap — maps screen rows to URLs
// ---------------------------------------------------------------------------

/// Maps terminal screen rows to hyperlink URLs.
///
/// Stores `row → URL`. On click, look up the clicked row.
/// When the terminal scrolls, call [`shift_up`] to adjust entries.
#[derive(Debug, Default)]
pub struct HyperlinkMap {
    rows: HashMap<u16, String>,
}

impl HyperlinkMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a hyperlink URL for the given screen row.
    pub fn set_row(&mut self, row: u16, url: &str) {
        self.rows.insert(row, url.to_string());
    }

    /// Look up the URL for a row, if any.
    pub fn get_row(&self, row: u16) -> Option<&str> {
        self.rows.get(&row).map(|s| s.as_str())
    }

    /// Shift all entries up by `n` rows (called when the terminal scrolls).
    pub fn shift_up(&mut self, n: u16) {
        let mut updated = HashMap::with_capacity(self.rows.len());
        for (row, url) in self.rows.drain() {
            if row >= n {
                updated.insert(row - n, url);
            }
        }
        self.rows = updated;
    }

    /// Remove all entries.
    pub fn clear(&mut self) {
        self.rows.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Osc8Filter tests --

    /// Issue #1705: an OSC 52 request, with either terminator, is lifted out of
    /// the stream whole — the text around it still reaches vt100, the request
    /// does not.
    #[test]
    fn clipboard_request_is_lifted_out_with_either_terminator() {
        let mut f = Osc8Filter::new();
        let segs = f.process(b"before\x1b]52;c;aGk=\x07middle\x1b]52;;eW8=\x1b\\after");
        assert_eq!(segs, vec![Osc8Segment::Text(b"beforemiddleafter".to_vec())]);
        assert_eq!(
            f.take_clipboard_requests(),
            vec![b"c;aGk=".to_vec(), b";eW8=".to_vec()]
        );
        assert!(
            f.take_clipboard_requests().is_empty(),
            "taken requests are gone"
        );
    }

    /// Issue #1705: a request split across PTY reads is still lifted out whole,
    /// and only once it completes.
    #[test]
    fn clipboard_request_split_across_chunks() {
        let mut f = Osc8Filter::new();
        assert!(f.process(b"\x1b]5").is_empty());
        assert!(f.process(b"2;c;aG").is_empty());
        assert!(f.take_clipboard_requests().is_empty(), "not complete yet");
        assert_eq!(f.process(b"k=\x1b"), vec![]);
        assert_eq!(f.process(b"\\x"), vec![Osc8Segment::Text(b"x".to_vec())]);
        assert_eq!(f.take_clipboard_requests(), vec![b"c;aGk=".to_vec()]);
    }

    /// Issue #1705: an OSC 52 request too large to ever be accepted is dropped
    /// whole and stops being buffered, and the stream after it is unharmed.
    #[test]
    fn oversized_clipboard_request_is_dropped_and_not_buffered() {
        let mut f = Osc8Filter::new();
        f.process(b"\x1b]52;c;");
        let chunk = vec![b'A'; 1024 * 1024];
        for _ in 0..4 {
            f.process(&chunk);
        }
        assert!(
            f.osc_other_buf.len() <= 2 * crate::osc52::OSC52_MAX_PAYLOAD_BYTES + 16,
            "buffered {} bytes of one request",
            f.osc_other_buf.len()
        );
        assert_eq!(
            f.process(b"\x07after"),
            vec![Osc8Segment::Text(b"after".to_vec())]
        );
        assert!(f.take_clipboard_requests().is_empty());
        // The next request is buffered normally again.
        f.process(b"\x1b]52;c;aGk=\x07");
        assert_eq!(f.take_clipboard_requests(), vec![b"c;aGk=".to_vec()]);
    }

    /// Issue #1705: an OSC 52 request cut off by another escape, or cancelled
    /// by CAN or SUB, is dropped and does not swallow the output after it.
    #[test]
    fn unterminated_clipboard_request_does_not_hide_later_output() {
        let mut f = Osc8Filter::new();
        assert_eq!(
            f.process(b"\x1b]52;c;aGk\x1b[31mred"),
            vec![Osc8Segment::Text(b"\x1b[31mred".to_vec())]
        );
        assert!(f.take_clipboard_requests().is_empty());

        let mut f = Osc8Filter::new();
        assert_eq!(
            f.process(b"\x1b]52;c;aGk\x18after"),
            vec![Osc8Segment::Text(b"\x18after".to_vec())]
        );
        assert!(f.take_clipboard_requests().is_empty());

        // A cut-off request followed at once by a complete one: the second is kept.
        let mut f = Osc8Filter::new();
        assert!(f.process(b"\x1b]52;c;aGk\x1b]52;c;eW8=\x07").is_empty());
        assert_eq!(f.take_clipboard_requests(), vec![b"c;eW8=".to_vec()]);
    }

    /// Issue #1705: only OSC 52 is lifted out; OSC 5, OSC 520 and a title
    /// still reach vt100 unchanged.
    #[test]
    fn other_oscs_still_pass_through() {
        let mut f = Osc8Filter::new();
        let input: &[u8] = b"\x1b]0;title\x07\x1b]5;x\x07\x1b]520;y\x07";
        assert_eq!(f.process(input), vec![Osc8Segment::Text(input.to_vec())]);
        assert!(f.take_clipboard_requests().is_empty());
    }

    #[test]
    fn plain_text_passes_through() {
        let mut f = Osc8Filter::new();
        let segs = f.process(b"Hello, world!");
        assert_eq!(segs, vec![Osc8Segment::Text(b"Hello, world!".to_vec())]);
    }

    #[test]
    fn single_link_bel() {
        let mut f = Osc8Filter::new();
        let segs = f.process(b"\x1b]8;;https://example.com\x07Click\x1b]8;;\x07");
        assert_eq!(
            segs,
            vec![Osc8Segment::LinkedText {
                url: "https://example.com".into(),
                bytes: b"Click".to_vec(),
            }]
        );
    }

    #[test]
    fn single_link_st() {
        let mut f = Osc8Filter::new();
        let segs = f.process(b"\x1b]8;;https://x.com\x1b\\Click\x1b]8;;\x1b\\");
        assert_eq!(
            segs,
            vec![Osc8Segment::LinkedText {
                url: "https://x.com".into(),
                bytes: b"Click".to_vec(),
            }]
        );
    }

    #[test]
    fn link_with_id_param() {
        let mut f = Osc8Filter::new();
        let segs = f.process(b"\x1b]8;id=foo;https://example.com\x07text\x1b]8;;\x07");
        assert_eq!(
            segs,
            vec![Osc8Segment::LinkedText {
                url: "https://example.com".into(),
                bytes: b"text".to_vec(),
            }]
        );
    }

    #[test]
    fn link_with_surrounding_text() {
        let mut f = Osc8Filter::new();
        let segs = f.process(b"before \x1b]8;;http://u\x07link\x1b]8;;\x07 after");
        assert_eq!(
            segs,
            vec![
                Osc8Segment::Text(b"before ".to_vec()),
                Osc8Segment::LinkedText {
                    url: "http://u".into(),
                    bytes: b"link".to_vec(),
                },
                Osc8Segment::Text(b" after".to_vec()),
            ]
        );
    }

    #[test]
    fn ansi_inside_link() {
        let mut f = Osc8Filter::new();
        let segs =
            f.process(b"\x1b]8;;http://u\x07\x1b[34m\x1b[1mblue bold\x1b[22m\x1b[39m\x1b]8;;\x07");
        assert_eq!(
            segs,
            vec![Osc8Segment::LinkedText {
                url: "http://u".into(),
                bytes: b"\x1b[34m\x1b[1mblue bold\x1b[22m\x1b[39m".to_vec(),
            }]
        );
    }

    #[test]
    fn non_osc8_passes_through() {
        let mut f = Osc8Filter::new();
        let segs = f.process(b"\x1b]0;title\x07hello");
        assert_eq!(
            segs,
            vec![Osc8Segment::Text(b"\x1b]0;title\x07hello".to_vec())]
        );
    }

    #[test]
    fn split_across_chunks() {
        let mut f = Osc8Filter::new();
        let s1 = f.process(b"\x1b]8;;https://exam");
        assert_eq!(s1, vec![]);
        let s2 = f.process(b"ple.com\x07link\x1b]8;;\x07");
        assert_eq!(
            s2,
            vec![Osc8Segment::LinkedText {
                url: "https://example.com".into(),
                bytes: b"link".to_vec(),
            }]
        );
    }

    #[test]
    fn regular_csi_passes_through() {
        let mut f = Osc8Filter::new();
        let segs = f.process(b"\x1b[31mred\x1b[0m");
        assert_eq!(
            segs,
            vec![Osc8Segment::Text(b"\x1b[31mred\x1b[0m".to_vec())]
        );
    }

    // -- HyperlinkMap tests --

    #[test]
    fn row_set_get() {
        let mut m = HyperlinkMap::new();
        m.set_row(5, "http://a");
        assert_eq!(m.get_row(5), Some("http://a"));
        assert_eq!(m.get_row(6), None);
    }

    #[test]
    fn row_shift_up() {
        let mut m = HyperlinkMap::new();
        m.set_row(0, "http://gone");
        m.set_row(3, "http://kept");
        m.shift_up(2);
        assert_eq!(m.get_row(0), None);
        assert_eq!(m.get_row(1), Some("http://kept"));
    }

    #[test]
    fn row_clear() {
        let mut m = HyperlinkMap::new();
        m.set_row(0, "http://a");
        m.clear();
        assert_eq!(m.get_row(0), None);
    }

    // -- Full pipeline integration test --

    #[test]
    fn full_pipeline() {
        let mut filter = Osc8Filter::new();
        let mut parser = vt100::Parser::new(24, 80, 0);
        let mut hmap = HyperlinkMap::new();

        let pty = b"Hello \x1b]8;;https://example.com\x07Click here\x1b]8;;\x07 world\r\n";
        let segments = filter.process(pty);

        for segment in &segments {
            match segment {
                Osc8Segment::Text(bytes) => {
                    parser.process(bytes);
                }
                Osc8Segment::LinkedText { url, bytes } => {
                    let row = parser.screen().cursor_position().0;
                    parser.process(bytes);
                    hmap.set_row(row, url);
                }
            }
        }

        let contents = parser.screen().contents();
        assert!(contents.contains("Hello Click here world"));
        assert_eq!(hmap.get_row(0), Some("https://example.com"));
        assert_eq!(hmap.get_row(1), None);
    }
}
