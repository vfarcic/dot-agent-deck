//! Which run a process belongs to, read from its environment, and which of
//! the collector's own resource pairs it may send.
//!
//! The deck writes a run's identity into each agent's
//! `OTEL_RESOURCE_ATTRIBUTES` with [`crate::telemetry::resource`]'s encoder,
//! so it is read back here with the same module's decoder: what the deck can
//! write, the collector can read.

use crate::telemetry::config::OTEL_RESOURCE_ATTRIBUTES;
use crate::telemetry::resource::{parse_resource_attributes, percent_decode};

use super::keys;

/// The keys of a run's identity, besides `deck.run.id`, that the collector
/// copies from a process's environment onto the run's resource. No other key
/// from a process's environment is ever sent.
pub const RUN_IDENTITY_KEYS: [&str; 5] = [
    "deck.pane.id",
    "deck.agent",
    "deck.role",
    "deck.orchestration.name",
    "deck.orchestration.id",
];

/// Keys the collector sets itself, which its own `OTEL_RESOURCE_ATTRIBUTES`
/// cannot replace.
const OWNED_KEYS: [&str; 3] = ["service.name", "service.version", "host.name"];

/// A run, as one process's environment names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity {
    pub run_id: String,
    /// Those of [`RUN_IDENTITY_KEYS`] the environment carries with a
    /// non-empty value, in that order.
    pub attributes: Vec<(String, String)>,
}

/// Bytes of a process's `OTEL_RESOURCE_ATTRIBUTES` value the collector
/// accepts. A longer value names no run, so the process is attributed by its
/// parent instead: the variable is the process's to write, and the collector
/// reads it from every visible process at every sample until it is settled.
pub const MAX_RESOURCE_ATTRIBUTES_BYTES: usize = 16 * 1024;

/// Bytes of one decoded identity value (`deck.run.id` or one of
/// [`RUN_IDENTITY_KEYS`]) the collector accepts. A longer value is left out,
/// never cut short: two run ids that differ only past the limit must not
/// become one run.
pub const MAX_IDENTITY_VALUE_BYTES: usize = 256;

/// The raw value of the first `OTEL_RESOURCE_ATTRIBUTES` entry in a raw
/// `/proc/<pid>/environ` (NUL-separated `KEY=value` entries), as `getenv`
/// would return it.
pub fn resource_attributes_in(environ: &[u8]) -> Option<&[u8]> {
    let prefix = format!("{OTEL_RESOURCE_ATTRIBUTES}=");
    environ
        .split(|b| *b == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
}

/// The run a raw environment names, or `None` when it carries no
/// `OTEL_RESOURCE_ATTRIBUTES`, a longer one than
/// [`MAX_RESOURCE_ATTRIBUTES_BYTES`], or no usable `deck.run.id` in it.
///
/// It decodes the list as the deck's `parse_resource_attributes` does (an
/// entry split at its first `=`, key and value trimmed and percent-decoded, an
/// entry with no `=` skipped, a later duplicate key replacing an earlier one,
/// bytes that are not UTF-8 replaced), but in one pass that keeps only the six
/// keys it needs. A value longer than [`MAX_IDENTITY_VALUE_BYTES`] once
/// decoded is left out: a run id, and with it the identity; another key, that
/// key alone. As a later duplicate it still replaces the earlier value.
pub fn run_identity(environ: &[u8]) -> Option<RunIdentity> {
    let raw = resource_attributes_in(environ)?;
    if raw.len() > MAX_RESOURCE_ATTRIBUTES_BYTES {
        return None;
    }
    let raw = String::from_utf8_lossy(raw);
    // Per key: `None` when not seen, `Some(None)` when its last value was
    // over the limit.
    let mut run_id: Option<Option<String>> = None;
    let mut found: [Option<Option<String>>; RUN_IDENTITY_KEYS.len()] = Default::default();
    for entry in raw.split(',') {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // A percent-encoded byte takes three, so a longer key cannot decode
        // to any of the six.
        if key.len() > 3 * LONGEST_KEY {
            continue;
        }
        let key = percent_decode(key);
        let slot = if key == keys::RUN_ID {
            &mut run_id
        } else if let Some(i) = RUN_IDENTITY_KEYS.iter().position(|k| *k == key) {
            &mut found[i]
        } else {
            continue;
        };
        let value = value.trim();
        *slot = Some(if value.len() > 3 * MAX_IDENTITY_VALUE_BYTES {
            None
        } else {
            Some(percent_decode(value)).filter(|v| v.len() <= MAX_IDENTITY_VALUE_BYTES)
        });
    }
    let run_id = run_id.flatten().filter(|v| !v.is_empty())?;
    let attributes = RUN_IDENTITY_KEYS
        .iter()
        .zip(found)
        .filter_map(|(key, value)| {
            let value = value.flatten().filter(|v| !v.is_empty())?;
            Some((key.to_string(), value))
        })
        .collect();
    Some(RunIdentity { run_id, attributes })
}

/// The longest of the keys [`run_identity`] keeps.
const LONGEST_KEY: usize = {
    let mut longest = keys::RUN_ID.len();
    let mut i = 0;
    while i < RUN_IDENTITY_KEYS.len() {
        if RUN_IDENTITY_KEYS[i].len() > longest {
            longest = RUN_IDENTITY_KEYS[i].len();
        }
        i += 1;
    }
    longest
};

/// The collector's own `OTEL_RESOURCE_ATTRIBUTES` pairs it adds to every
/// resource: all of them except a `deck.` key and the keys it owns. A
/// collector started from inside a deck pane inherits that pane's
/// `deck.run.id`, which must not label every series it sends.
pub fn own_resource_pairs(raw: Option<&str>) -> Vec<(String, String)> {
    raw.map(parse_resource_attributes)
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| !k.starts_with("deck.") && !OWNED_KEYS.contains(&k.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::resource::merge_resource_attributes;

    fn environ(entries: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in entries {
            out.extend_from_slice(entry.as_bytes());
            out.push(0);
        }
        out
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn reads_the_run_and_only_the_allow_listed_identity() {
        let env = environ(&[
            "PATH=/bin",
            "OTEL_RESOURCE_ATTRIBUTES=team=x,deck.run.id=run-1,deck.pane.id=7,deck.agent=claude-code,secret=s,deck.role=code%20reviewer",
            "HOME=/home/u",
        ]);
        assert_eq!(
            run_identity(&env),
            Some(RunIdentity {
                run_id: "run-1".into(),
                attributes: pairs(&[
                    ("deck.pane.id", "7"),
                    ("deck.agent", "claude-code"),
                    ("deck.role", "code reviewer"),
                ]),
            })
        );
    }

    #[test]
    fn decodes_what_the_deck_encodes() {
        let written = merge_resource_attributes(
            Some("team=a,b"),
            &pairs(&[
                ("deck.run.id", "id,with=awkward;bytes ✓%"),
                ("deck.orchestration.name", "tdd, fast"),
            ]),
        );
        let env = environ(&[&format!("OTEL_RESOURCE_ATTRIBUTES={written}")]);
        let identity = run_identity(&env).unwrap();
        assert_eq!(identity.run_id, "id,with=awkward;bytes ✓%");
        assert_eq!(
            identity.attributes,
            pairs(&[("deck.orchestration.name", "tdd, fast")])
        );
    }

    #[test]
    fn edge_cases_of_the_list() {
        let id = |raw: &str| {
            run_identity(&environ(&[&format!("OTEL_RESOURCE_ATTRIBUTES={raw}")])).map(|i| i.run_id)
        };
        // A later duplicate replaces an earlier one, as in the deck.
        assert_eq!(id("deck.run.id=a,deck.run.id=b").as_deref(), Some("b"));
        // An `=` inside a value belongs to the value.
        assert_eq!(id("deck.run.id=a=b").as_deref(), Some("a=b"));
        // Whitespace around keys and values is trimmed.
        assert_eq!(id(" deck.run.id = r1 , x=y").as_deref(), Some("r1"));
        // An encoded key is decoded before it is matched.
        assert_eq!(id("deck%2Erun%2Eid=r2").as_deref(), Some("r2"));
        // Entries with no `=` or an empty key are skipped.
        assert_eq!(id("junk,=v,deck.run.id=r3").as_deref(), Some("r3"));
        // A malformed escape is kept as written.
        assert_eq!(id("deck.run.id=100%").as_deref(), Some("100%"));
        // No usable id.
        assert_eq!(id("deck.run.id="), None);
        assert_eq!(id("deck.run.id=   "), None);
        assert_eq!(id("team=x"), None);
        assert_eq!(id(""), None);
    }

    #[test]
    fn the_first_entry_wins_and_lookalikes_do_not_count() {
        let env = environ(&[
            "XOTEL_RESOURCE_ATTRIBUTES=deck.run.id=no",
            "OTEL_RESOURCE_ATTRIBUTES_EXTRA=deck.run.id=no",
            "OTEL_RESOURCE_ATTRIBUTES=deck.run.id=first",
            "OTEL_RESOURCE_ATTRIBUTES=deck.run.id=second",
        ]);
        assert_eq!(run_identity(&env).unwrap().run_id, "first");
        assert_eq!(run_identity(b""), None);
        assert_eq!(run_identity(&environ(&["PATH=/bin"])), None);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        let mut env = b"OTEL_RESOURCE_ATTRIBUTES=deck.run.id=r\xff1".to_vec();
        env.push(0);
        assert_eq!(run_identity(&env).unwrap().run_id, "r\u{FFFD}1");
    }

    /// A hostile environment: thousands of distinct keys before the run id
    /// are walked once, and the identity is what the six keys say.
    #[test]
    fn many_unrelated_keys_are_skipped_in_one_pass() {
        let mut raw: String = (0..1_500).map(|i| format!("k{i}=v,")).collect();
        raw.push_str("deck.run.id=r1,deck.pane.id=p");
        assert!(raw.len() <= MAX_RESOURCE_ATTRIBUTES_BYTES, "{}", raw.len());
        let env = environ(&[&format!("OTEL_RESOURCE_ATTRIBUTES={raw}")]);
        assert_eq!(
            run_identity(&env),
            Some(RunIdentity {
                run_id: "r1".into(),
                attributes: pairs(&[("deck.pane.id", "p")]),
            })
        );
        // One byte past the cap names no run, wherever the run id is.
        let over = format!(
            "deck.run.id=r1,pad={}",
            "x".repeat(MAX_RESOURCE_ATTRIBUTES_BYTES)
        );
        let env = environ(&[&format!("OTEL_RESOURCE_ATTRIBUTES={over}")]);
        assert_eq!(run_identity(&env), None);
    }

    /// An over-limit value is left out, never cut short, so two long ids that
    /// share a prefix cannot become one run.
    #[test]
    fn an_over_limit_value_is_omitted_not_truncated() {
        let id = |raw: &str| run_identity(&environ(&[&format!("OTEL_RESOURCE_ATTRIBUTES={raw}")]));
        let at_limit = "a".repeat(MAX_IDENTITY_VALUE_BYTES);
        assert_eq!(
            id(&format!("deck.run.id={at_limit}")).map(|i| i.run_id),
            Some(at_limit.clone())
        );
        let over = format!("{at_limit}b");
        assert_eq!(id(&format!("deck.run.id={over}")), None);
        // Encoded, it is the decoded length that counts.
        let encoded_over = "%61".repeat(MAX_IDENTITY_VALUE_BYTES + 1);
        assert_eq!(id(&format!("deck.run.id={encoded_over}")), None);
        let encoded_at = "%61".repeat(MAX_IDENTITY_VALUE_BYTES);
        assert_eq!(
            id(&format!("deck.run.id={encoded_at}")).map(|i| i.run_id),
            Some(at_limit.clone())
        );
        // A later over-limit duplicate still replaces the earlier value.
        assert_eq!(id(&format!("deck.run.id=short,deck.run.id={over}")), None);
        // Another key over the limit is dropped alone.
        assert_eq!(
            id(&format!("deck.run.id=r1,deck.role={over},deck.agent=a")),
            Some(RunIdentity {
                run_id: "r1".into(),
                attributes: pairs(&[("deck.agent", "a")]),
            })
        );
    }

    #[test]
    fn own_pairs_drop_deck_keys_and_owned_keys() {
        assert_eq!(
            own_resource_pairs(Some(
                "team=x,deck.run.id=forged,service.name=s,host.name=h,service.version=v,deck.pane.id=1,env=dev%20box"
            )),
            pairs(&[("team", "x"), ("env", "dev box")])
        );
        assert!(own_resource_pairs(None).is_empty());
    }
}
