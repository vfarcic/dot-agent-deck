//! Which run a process belongs to, read from its environment, and which of
//! the collector's own resource pairs it may send.
//!
//! The deck writes a run's identity into each agent's
//! `OTEL_RESOURCE_ATTRIBUTES` with [`crate::telemetry::resource`]'s encoder,
//! so it is read back here with the same module's decoder: what the deck can
//! write, the collector can read.

use crate::telemetry::config::OTEL_RESOURCE_ATTRIBUTES;
use crate::telemetry::resource::parse_resource_attributes;

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

/// The value of the first `OTEL_RESOURCE_ATTRIBUTES` entry in a raw
/// `/proc/<pid>/environ` (NUL-separated `KEY=value` entries), as `getenv`
/// would return it. Bytes that are not UTF-8 are replaced.
pub fn resource_attributes_in(environ: &[u8]) -> Option<String> {
    let prefix = format!("{OTEL_RESOURCE_ATTRIBUTES}=");
    environ
        .split(|b| *b == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .map(|value| String::from_utf8_lossy(value).into_owned())
}

/// The run a raw environment names, or `None` when it carries no
/// `OTEL_RESOURCE_ATTRIBUTES` or no non-empty `deck.run.id` in it.
pub fn run_identity(environ: &[u8]) -> Option<RunIdentity> {
    let pairs = parse_resource_attributes(&resource_attributes_in(environ)?);
    let value = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    let run_id = value(keys::RUN_ID)?.to_string();
    let attributes = RUN_IDENTITY_KEYS
        .iter()
        .filter_map(|key| value(key).map(|v| (key.to_string(), v.to_string())))
        .collect();
    Some(RunIdentity { run_id, attributes })
}

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
