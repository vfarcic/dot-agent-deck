//! The Rust half of the endpoint-field contract (PRD #741 M7).
//!
//! The webview validates a deck row *before* it has anything to save, so the
//! Decks panel can say "that is not a hostname" while the user types instead of
//! after a round trip. Those predicates live in `desktop/src/lib/endpoints.ts`
//! and the authority they stand in for lives here: the five
//! [`dot_agent_deck::remote_tunnel`] newtypes, whose `Deserialize` runs the same
//! check their constructors do.
//!
//! Two implementations of one rule, in two languages, is a drift waiting to
//! happen — and it had already happened. Measured on the shipped build: the
//! webview's universal refusals held `* ? [ ] { } ~ ! # ^`, none of which Rust
//! refuses universally, and because that set was tested *before* each field's
//! own charset it overrode all five of them. The Key file input's own
//! placeholder `~/.ssh/id_ed25519` and the bracketed IPv6 literal the Host
//! input's error text suggests were therefore both rejected by the field that
//! suggested them, which disabled `Test connection` — M10's only path to
//! discovering a deck's socket. Three more disagreements rode along: the jump
//! charset accepted an `@` [`HostAlias`] refuses, the host charset omitted the
//! `_` and `%` [`Hostname`] accepts, and one 255-byte cap stood in for
//! [`SshUser`]'s 64, [`KeyPath`]'s 4096 and [`HostAlias`]'s 253. None of it was
//! tested on either side; the file's own comment asserted the relationship
//! instead.
//!
//! # How the two are held together
//!
//! `desktop/src/lib/endpointValidationCases.json` is a table of values and the
//! verdict each field owes them. It is read **twice**: `endpoints.test.ts`
//! asserts the TypeScript predicates decide those verdicts, and the tests below
//! assert the Rust deserializers decide the same ones. Neither side owns the
//! expectations, so a predicate that drifts from its newtype reddens a gate
//! rather than shipping.
//!
//! It is a shared table rather than one test calling both implementations,
//! because crossing the boundary for real means a Rust test shelling out to
//! `node` over compiled TypeScript — a toolchain and a build step inside
//! `cargo test-fast`, for a check two readers of one file already make. What
//! the stronger form would add is that a *drift in the file itself* is caught;
//! what it costs is a node dependency on the fast tier. The two guard tests
//! below buy back most of that: the table must name all five fields with both
//! verdicts for each, and it must still carry the two placeholder rows.
//!
//! The table is compiled in with [`include_str!`], so it is a build input
//! rather than a path resolved at run time — a missing or moved file fails the
//! build, and `cargo` re-runs these tests when the JSON changes.
//!
//! # The one deliberate divergence
//!
//! Rust refuses the empty string for all five types: a stored field that exists
//! must have a value. The webview reads an empty *optional* field as **unset**
//! and `EndpointsPanel.tsx` maps it back to `undefined` before a save, so no
//! newtype ever sees one. The table deliberately holds no empty row, and the
//! two sides pin that disagreement in a named test each.

#[cfg(test)]
mod tests {
    use dot_agent_deck::remote_tunnel::{HostAlias, Hostname, KeyPath, RemoteSocketPath, SshUser};
    use serde::Deserialize;
    use serde::de::DeserializeOwned;

    /// The shared table, as a build input. The path is relative to this file:
    /// `desktop/src-tauri/src/../../src/lib/endpointValidationCases.json`.
    const CASES_JSON: &str = include_str!("../../src/lib/endpointValidationCases.json");

    /// Every field the Decks panel validates, spelled as the table spells it.
    const FIELDS: [&str; 5] = ["host", "user", "identity", "jump", "socket"];

    /// The placeholders that are specimen values — text a user could type
    /// unchanged — paired with the field that displays them. Both are rows of
    /// the table, and both were rejected by their own field as shipped.
    const SPECIMEN_PLACEHOLDERS: [(&str, &str); 2] = [
        ("host", "build-box.example.com"),
        ("identity", "~/.ssh/id_ed25519"),
    ];

    #[derive(Debug, Deserialize)]
    struct Table {
        cases: Vec<Case>,
    }

    #[derive(Debug, Deserialize)]
    struct Case {
        /// Which of [`FIELDS`] this row is about.
        field: String,
        /// The value, before [`Case::value`] applies `repeat`.
        value: String,
        /// How many times `value` repeats, so a 4096-byte bound can be pinned
        /// without a 4096-character literal. Absent means once.
        #[serde(default)]
        repeat: Option<usize>,
        /// What both sides owe this value.
        accepted: bool,
        /// Why, in the table's own words — quoted back in a failure so the
        /// reader does not have to open the JSON to understand one.
        why: String,
    }

    impl Case {
        fn value(&self) -> String {
            self.value.repeat(self.repeat.unwrap_or(1))
        }

        /// The value, short enough to read in a failure message.
        fn describe(&self) -> String {
            match self.repeat {
                None | Some(1) => format!("{:?}", self.value),
                Some(times) => format!("{:?} x {times}", self.value),
            }
        }
    }

    fn table() -> Table {
        serde_json::from_str(CASES_JSON).expect("the shared validation table is valid JSON")
    }

    /// Whether `raw` survives `T`'s **deserializer** — the gate a hand-edited
    /// `desktop.toml` and a settings save both go through, rather than the
    /// `parse` constructor it happens to wrap.
    fn accepts<T: DeserializeOwned>(raw: &str) -> bool {
        serde_json::from_value::<T>(serde_json::Value::String(raw.to_string())).is_ok()
    }

    fn rust_accepts(field: &str, raw: &str) -> bool {
        match field {
            "host" => accepts::<Hostname>(raw),
            "user" => accepts::<SshUser>(raw),
            "identity" => accepts::<KeyPath>(raw),
            "jump" => accepts::<HostAlias>(raw),
            "socket" => accepts::<RemoteSocketPath>(raw),
            other => panic!(
                "the shared table names the field {other:?}, which no newtype here owns; \
                 the fields are {FIELDS:?}"
            ),
        }
    }

    /// Every row of the shared table, decided by the newtype that owns its
    /// field. `endpoints.test.ts` asserts the TypeScript predicates decide the
    /// same rows the same way, which is what makes the pair a contract rather
    /// than two opinions.
    #[test]
    fn every_row_of_the_shared_table_is_decided_the_way_this_side_decides_it() {
        let mut wrong = Vec::new();
        for case in table().cases {
            let value = case.value();
            let accepted = rust_accepts(&case.field, &value);
            if accepted != case.accepted {
                wrong.push(format!(
                    "  {} {}: the table says {}, this side {} it ({})",
                    case.field,
                    case.describe(),
                    if case.accepted { "accepted" } else { "refused" },
                    if accepted { "accepts" } else { "refuses" },
                    case.why,
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "the shared endpoint-validation table disagrees with this side:\n{}\n\n\
             Rust is the authority for what a stored endpoint may be, so a row that is \
             wrong about Rust is a wrong row: fix the table and the TypeScript predicate \
             in desktop/src/lib/endpoints.ts together.",
            wrong.join("\n"),
        );
    }

    /// The table has to stay a table of all five fields.
    ///
    /// Without this it could be gutted a field at a time and every remaining
    /// row would still pass — which is close to where this started, since the
    /// TypeScript predicates had no test at all.
    #[test]
    fn the_shared_table_names_every_field_with_both_verdicts_for_each() {
        let cases = table().cases;
        for field in FIELDS {
            let rows: Vec<&Case> = cases.iter().filter(|case| case.field == field).collect();
            assert!(
                rows.iter().any(|case| case.accepted),
                "the shared table has no accepted case for {field}",
            );
            assert!(
                rows.iter().any(|case| !case.accepted),
                "the shared table has no refused case for {field}",
            );
        }
        for case in &cases {
            assert!(
                FIELDS.contains(&case.field.as_str()),
                "the shared table names the field {:?}, which is not one of {FIELDS:?}",
                case.field,
            );
        }
    }

    /// The two placeholders the panel displays as specimen values stay in the
    /// table, accepted.
    ///
    /// This is the row that would have caught the defect: both were rejected by
    /// the field that displayed them, and with `rowProblems` non-empty the
    /// `Test connection` button — M10's only route to discovering a deck's
    /// socket path — was disabled. The TypeScript side additionally holds the
    /// panel's own `FIELD_PLACEHOLDERS` to these values, so the pair cannot
    /// drift apart by editing the panel either.
    #[test]
    fn the_placeholders_the_panel_displays_are_pinned_as_accepted_rows() {
        let cases = table().cases;
        for (field, placeholder) in SPECIMEN_PLACEHOLDERS {
            let row = cases
                .iter()
                .find(|case| case.field == field && case.value() == placeholder)
                .unwrap_or_else(|| {
                    panic!(
                        "the shared table has no row for the {field} field's own placeholder \
                         {placeholder:?}; it is there so neither side can reject the text the \
                         UI itself suggests"
                    )
                });
            assert!(
                row.accepted,
                "the {field} field's own placeholder {placeholder:?} is listed as refused",
            );
            assert!(
                rust_accepts(field, placeholder),
                "the {field} field's own placeholder {placeholder:?} is refused by this side",
            );
        }
    }

    /// The empty string: refused here, read as *unset* on the webview's side.
    ///
    /// The single place the two disagree, and deliberately. A stored field that
    /// exists must have a value, so every newtype refuses `""`; the panel reads
    /// an empty optional input as "not set" and stores `undefined`, so no
    /// newtype ever sees one. `endpoints.test.ts`'s "reads an empty optional
    /// field as unset" is the other half of this pair, and the shared table
    /// carries no empty row because a row would have to claim one verdict for
    /// both.
    /// The sixth field, whose rule is a numeric range rather than a charset
    /// (PRD #741, Greptile P2 on #1035).
    ///
    /// **Not a row of the shared table, and that is a shape argument rather
    /// than an omission**: every row there is a string handed to a `String`
    /// deserializer, and the port is a `u16` handed to a `type="number"` input.
    /// The two sides are held together by this test and by `portProblem`'s in
    /// `endpoints.test.ts`, which name the same four boundary values in the
    /// same terms.
    ///
    /// What it is here to stop coming back: `port` was a bare `u16` until this
    /// milestone, so Rust accepted `0` and the webview did not — the only value
    /// the two disagreed on, and not an inert one, since a hand-edited
    /// `desktop.toml` carrying it reached OpenSSH as `-p 0`.
    #[test]
    fn the_port_rule_this_side_applies_is_the_one_ssh_can_use() {
        use dot_agent_deck::remote_tunnel::SshPort;

        for accepted in [1u16, 22, 2222, 65535] {
            assert!(
                SshPort::parse(accepted).is_ok(),
                "the panel offers {accepted}, so this side must take it"
            );
            assert_eq!(
                serde_json::from_str::<SshPort>(&accepted.to_string())
                    .expect("a stored port in range must load")
                    .get(),
                accepted,
                "a hand-written document must round-trip the value it holds"
            );
        }

        assert!(
            SshPort::parse(0).is_err(),
            "the webview refuses 0 and ssh refuses `-p 0`, so this side must refuse it too"
        );
        let refused = serde_json::from_str::<SshPort>("0")
            .expect_err("a stored `port = 0` must not load at all");
        assert!(
            refused.to_string().contains("1 and 65535"),
            "the refusal must name the range a user can act on: {refused}"
        );
        // 65536 is unrepresentable in the underlying `u16`, so the deserializer
        // refuses it before `SshPort` is consulted — the same verdict by a
        // different route, which is why there is no `parse` case for it.
        assert!(serde_json::from_str::<SshPort>("65536").is_err());
        assert_eq!(SshPort::DEFAULT.get(), 22);
    }

    #[test]
    fn the_empty_string_is_refused_here_while_the_panel_reads_it_as_unset() {
        for field in FIELDS {
            assert!(
                !rust_accepts(field, ""),
                "{field} accepted the empty string; the webview reads an empty optional \
                 field as unset on the understanding that this side refuses one",
            );
        }
    }
}
