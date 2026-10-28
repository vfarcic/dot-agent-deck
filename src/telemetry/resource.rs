//! `OTEL_RESOURCE_ATTRIBUTES` in both directions, the resource the deck's own
//! span batches carry, and the machine's host name.
//!
//! The variable's format is the specification's: `key=value` pairs separated by
//! commas, with values percent-encoded as W3C Baggage encodes them. Decoding
//! accepts any `%XX` sequence; encoding escapes every byte outside Baggage's
//! `baggage-octet` set, which is what an SDK reading the merged value will
//! decode back to the original string.

use super::AttrKey;
use super::span::{AttrValue, Attribute};

/// Decode a `key=value,key2=value2` list (`OTEL_RESOURCE_ATTRIBUTES`,
/// `OTEL_EXPORTER_OTLP_HEADERS`). Keys and values are trimmed and
/// percent-decoded; an entry with no `=` or an empty key is skipped.
pub fn parse_key_value_list(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            let key = percent_decode(key.trim());
            if key.is_empty() {
                return None;
            }
            Some((key, percent_decode(value.trim())))
        })
        .collect()
}

/// [`parse_key_value_list`] for `OTEL_RESOURCE_ATTRIBUTES`, where a later
/// duplicate key replaces an earlier one.
pub fn parse_resource_attributes(raw: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, value) in parse_key_value_list(raw) {
        if let Some(existing) = out.iter_mut().find(|(k, _)| *k == key) {
            existing.1 = value;
        } else {
            out.push((key, value));
        }
    }
    out
}

/// Encode pairs back into an `OTEL_RESOURCE_ATTRIBUTES` value. Values are
/// percent-encoded; keys are too, so a key a user wrote with a `,` or `=` in
/// it cannot split the list.
pub fn encode_resource_attributes(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join(",")
}

/// The user's `OTEL_RESOURCE_ATTRIBUTES` (if any) with `deck_pairs` merged in:
/// a key the deck sets replaces the user's value for it, so the identity an
/// agent reports is always the deck's, and every other user pair is kept in its
/// original order.
pub fn merge_resource_attributes(user: Option<&str>, deck_pairs: &[(String, String)]) -> String {
    let mut merged: Vec<(String, String)> = user
        .map(parse_resource_attributes)
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| !deck_pairs.iter().any(|(dk, _)| dk == k))
        .collect();
    merged.extend(deck_pairs.iter().cloned());
    encode_resource_attributes(&merged)
}

/// W3C Baggage `baggage-octet`: printable ASCII except space, `"`, `,`, `;`,
/// `\`. `%` is escaped too, so a literal `%` survives the round trip.
fn is_baggage_octet(b: u8) -> bool {
    matches!(b, 0x21 | 0x23..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E) && b != b'%'
}

pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if is_baggage_octet(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decode `%XX` sequences. A `%` not followed by two hex digits is kept as
/// written, and bytes that do not form UTF-8 are replaced, not rejected.
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// This machine's host name, or `None` when the OS reports none. Computed once.
pub fn host_name() -> Option<String> {
    static HOST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOST.get_or_init(read_host_name).clone()
}

#[cfg(unix)]
fn read_host_name() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its whole length, which is what we pass;
    // the result is read only up to the first NUL, and the last byte is never
    // written past.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(not(unix))]
fn read_host_name() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// The resource on every span batch the deck exports: the user's
/// `OTEL_RESOURCE_ATTRIBUTES` pairs, then the four the deck owns, which a user
/// pair of the same key does not override. `service.name` is the one the user
/// controls, through `OTEL_SERVICE_NAME` ([`super::TelemetryConfig::service_name`]).
pub fn deck_resource(
    service_name: &str,
    host: Option<&str>,
    user: &[(String, String)],
) -> Vec<ResourceAttribute> {
    let owned = [
        AttrKey::ServiceName,
        AttrKey::ServiceVersion,
        AttrKey::HostName,
        AttrKey::SchemaVersion,
    ];
    let mut out: Vec<ResourceAttribute> = user
        .iter()
        .filter(|(k, _)| !owned.iter().any(|o| o.as_str() == k))
        .map(|(k, v)| ResourceAttribute::User(k.clone(), v.clone()))
        .collect();
    out.push(ResourceAttribute::Deck(Attribute::str(
        AttrKey::ServiceName,
        service_name,
    )));
    out.push(ResourceAttribute::Deck(Attribute::str(
        AttrKey::ServiceVersion,
        env!("DAD_VERSION"),
    )));
    if let Some(host) = host {
        out.push(ResourceAttribute::Deck(Attribute::str(
            AttrKey::HostName,
            host,
        )));
    }
    out.push(ResourceAttribute::Deck(Attribute::str(
        AttrKey::SchemaVersion,
        super::SCHEMA_VERSION,
    )));
    out
}

/// One resource attribute: an allow-listed one the deck sets, or one the user
/// configured in `OTEL_RESOURCE_ATTRIBUTES`.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceAttribute {
    Deck(Attribute),
    User(String, String),
}

impl ResourceAttribute {
    pub fn key(&self) -> &str {
        match self {
            ResourceAttribute::Deck(a) => a.key.as_str(),
            ResourceAttribute::User(k, _) => k,
        }
    }

    pub fn value(&self) -> AttrValue {
        match self {
            ResourceAttribute::Deck(a) => a.value.clone(),
            ResourceAttribute::User(_, v) => AttrValue::Str(v.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn encode_then_parse_round_trips_awkward_values() {
        let original = pairs(&[
            ("deck.run.id", "run-1"),
            ("deck.role", "code reviewer, senior"),
            ("odd", "a=b;c\\d\"e%f ✓"),
            ("team", "platform"),
        ]);
        let encoded = encode_resource_attributes(&original);
        assert!(!encoded.contains(' '), "{encoded}");
        assert_eq!(encoded.matches(',').count(), original.len() - 1);
        assert_eq!(parse_resource_attributes(&encoded), original);
    }

    #[test]
    fn merge_keeps_user_pairs_and_lets_the_deck_identity_win() {
        let merged = merge_resource_attributes(
            Some("team=platform, deck.run.id=forged ,env=dev%20box"),
            &pairs(&[("deck.run.id", "run-42"), ("host.name", "mini")]),
        );
        assert_eq!(
            parse_resource_attributes(&merged),
            pairs(&[
                ("team", "platform"),
                ("env", "dev box"),
                ("deck.run.id", "run-42"),
                ("host.name", "mini"),
            ])
        );
        assert_eq!(
            merge_resource_attributes(None, &pairs(&[("a", "b c")])),
            "a=b%20c"
        );
    }

    #[test]
    fn decode_tolerates_malformed_escapes_and_entries() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(
            parse_key_value_list("novalue, =empty-key,ok=1"),
            pairs(&[("ok", "1")])
        );
    }

    #[test]
    fn deck_owned_keys_are_not_overridden_by_the_user() {
        let resource = deck_resource(
            "dot-agent-deck",
            Some("studio"),
            &pairs(&[
                ("service.name", "theirs"),
                ("host.name", "spoofed"),
                ("deck.telemetry.schema_version", "99"),
                ("team", "platform"),
            ]),
        );
        let get = |key: &str| {
            resource
                .iter()
                .filter(|a| a.key() == key)
                .map(|a| a.value())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            get("service.name"),
            vec![AttrValue::Str("dot-agent-deck".into())]
        );
        assert_eq!(get("host.name"), vec![AttrValue::Str("studio".into())]);
        assert_eq!(
            get("deck.telemetry.schema_version"),
            vec![AttrValue::Str("3".into())]
        );
        assert_eq!(get("team"), vec![AttrValue::Str("platform".into())]);
        assert_eq!(get("service.version").len(), 1);
    }

    #[test]
    fn host_name_is_never_empty() {
        assert!(host_name().is_none_or(|h| !h.is_empty()));
    }
}
