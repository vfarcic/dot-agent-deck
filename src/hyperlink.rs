//! OSC 8 hyperlink parsing and row-based URL tracking.
//!
//! Terminal applications emit OSC 8 escape sequences to create clickable
//! hyperlinks.  The vt100 crate does not support OSC 8, so this module
//! intercepts the raw PTY byte stream, strips OSC 8 sequences, and records
//! which screen rows have hyperlink URLs.

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
        }
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
                FilterState::Esc => {
                    if byte == b']' {
                        self.osc_num.clear();
                        self.state = FilterState::OscDigits;
                    } else {
                        self.text_buf.push(0x1b);
                        self.text_buf.push(byte);
                        self.state = FilterState::Normal;
                    }
                }
                FilterState::OscDigits => {
                    if byte.is_ascii_digit() {
                        self.osc_num.push(byte);
                    } else if byte == b';' && self.osc_num == b"8" {
                        self.flush_text(&mut segments);
                        self.osc_params.clear();
                        self.state = FilterState::Osc8Params;
                    } else {
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
                        self.osc_other_buf.push(byte);
                        self.text_buf
                            .extend_from_slice(&std::mem::take(&mut self.osc_other_buf));
                        self.state = FilterState::Normal;
                    } else if byte == 0x1b {
                        self.state = FilterState::OscOtherEsc;
                    } else {
                        self.osc_other_buf.push(byte);
                    }
                }
                FilterState::OscOtherEsc => {
                    if byte == b'\\' {
                        self.osc_other_buf.push(0x1b);
                        self.osc_other_buf.push(b'\\');
                        self.text_buf
                            .extend_from_slice(&std::mem::take(&mut self.osc_other_buf));
                        self.state = FilterState::Normal;
                    } else {
                        self.osc_other_buf.push(0x1b);
                        self.osc_other_buf.push(byte);
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
