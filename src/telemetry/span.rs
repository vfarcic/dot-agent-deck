//! The finished-span model the tracker produces and the encoders read.

use super::AttrKey;

/// An attribute value. No float variant: nothing the deck sends is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

/// One allow-listed attribute. The key is an [`AttrKey`], so an attribute
/// outside the allow-list cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub key: AttrKey,
    pub value: AttrValue,
}

impl Attribute {
    pub fn str(key: AttrKey, value: impl Into<String>) -> Self {
        Attribute {
            key,
            value: AttrValue::Str(value.into()),
        }
    }

    pub fn int(key: AttrKey, value: i64) -> Self {
        Attribute {
            key,
            value: AttrValue::Int(value),
        }
    }

    pub fn bool(key: AttrKey, value: bool) -> Self {
        Attribute {
            key,
            value: AttrValue::Bool(value),
        }
    }

    /// An optional string attribute: absent when the value is unknown or empty,
    /// never an empty string.
    pub fn opt_str(key: AttrKey, value: Option<&str>) -> Option<Self> {
        value
            .filter(|v| !v.trim().is_empty())
            .map(|v| Attribute::str(key, v))
    }
}

pub type TraceId = [u8; 16];
pub type SpanId = [u8; 8];

/// A span event (`deck.status_change`, `deck.intervention`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanEventRecord {
    pub name: &'static str,
    pub time_unix_nanos: u64,
    pub attributes: Vec<Attribute>,
}

/// A finished span, ready to encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanRecord {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    pub name: &'static str,
    pub start_unix_nanos: u64,
    pub end_unix_nanos: u64,
    pub attributes: Vec<Attribute>,
    pub events: Vec<SpanEventRecord>,
}

/// Random ids from the OS. A trace or span id of all zeroes is invalid in
/// OTLP, so one is replaced; an OS that cannot supply randomness falls back to
/// the clock and a counter, which is still unique within this process.
pub fn new_trace_id() -> TraceId {
    let mut id = [0u8; 16];
    fill_random(&mut id);
    id
}

pub fn new_span_id() -> SpanId {
    let mut id = [0u8; 8];
    fill_random(&mut id);
    id
}

fn fill_random(buf: &mut [u8]) {
    if getrandom::fill(buf).is_err() || buf.iter().all(|b| *b == 0) {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let seed = super::now_unix_nanos() ^ seq.rotate_left(32);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed.rotate_left((i as u32 * 8) % 64) & 0xff) as u8 ^ (seq as u8);
        }
        if buf.iter().all(|b| *b == 0) {
            buf[0] = 1;
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh `deck.run.id`: 16 random bytes as lowercase hex.
pub fn new_run_id() -> String {
    hex(&new_trace_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_non_zero() {
        let a = new_trace_id();
        let b = new_trace_id();
        assert_ne!(a, b);
        assert!(a.iter().any(|x| *x != 0));
        assert_eq!(new_run_id().len(), 32);
        assert_ne!(new_span_id(), [0u8; 8]);
    }

    #[test]
    fn optional_strings_are_absent_rather_than_empty() {
        assert!(Attribute::opt_str(AttrKey::Role, None).is_none());
        assert!(Attribute::opt_str(AttrKey::Role, Some("  ")).is_none());
        assert_eq!(
            Attribute::opt_str(AttrKey::Role, Some("coder")),
            Some(Attribute::str(AttrKey::Role, "coder"))
        );
    }
}
