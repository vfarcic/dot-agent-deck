//! Issue #1116 — the guard that keeps [`crate::dto::DeckScope`] from decaying
//! into a convention.
//!
//! # The defect class
//!
//! The applied selection is mutable process state. Reading it answers *what is
//! selected now*, which is a different question from *which deck is my
//! operation about* — and the two diverge exactly when an `await` sits between
//! them. Three instances shipped on this branch, and each was found by a
//! separate audit round rather than by the round before it:
//!
//! | site | the two reads |
//! | --- | --- |
//! | `DesktopAction::StopAgent` | `trusted_daemon` before `stop_agent().await`, then `selected_endpoint()` **again** for the cleanup after it — so the daemon stopped deck A's agent and the cleanup detached deck B's namesake |
//! | `crate::terminal::attach` | `observed_decks()` before a process-wide gate, then a handshake and a stream attach with no recheck — so an attach queued behind a slower one could publish a session for a deck the user had removed while it waited |
//! | `crate::daemon_bridge::bootstrap` | the selection three times across two awaits — the snapshot that decides to lazy-spawn, the address to spawn at, and the snapshot it answers with |
//!
//! Each round fixed the instances it found. What no round fixed is that the
//! *shape* is invisible: `selected_endpoint()` reads like a property of the
//! operation rather than a question about a global, so a second call looks like
//! a repeat of the first answer instead of a fresh one.
//!
//! # What this module does about it
//!
//! [`crate::dto::DeckScope`] makes the captured value and the live value
//! different expressions — `scope.endpoint()` against `selected_endpoint()` —
//! so mixing them is legible at the call site. This module keeps the remaining
//! raw readers **counted**: the test below pins how many live in each consumer
//! module, so a new one reddens `cargo test-fast` until its author either
//! routes through a scope or raises the number deliberately, having read the
//! question in [`BUDGET`]'s doc comment.
//!
//! **This is a speed bump, not a proof, and it is worth saying which.** It
//! cannot stop a function from capturing two scopes, and it counts per *module*
//! rather than per *function* — the per-function count is what the defect is
//! really about, and attributing a hit to its enclosing `fn` needs a Rust
//! parser or a brittle indentation heuristic. What a guard most needs to catch
//! is a **new** reader appearing, and that it does catch; the author of that
//! reader is then the person who reads this file, which is the whole mechanism.
//!
//! The counts are the third audit's sweep, executed rather than recorded: a
//! grep pasted into a report goes stale the day after, and this one re-runs on
//! every `cargo test-fast`.
//!
//! # What is scanned, and what deliberately is not
//!
//! Only the **consumers**. `dto.rs` owns the state and implements `DeckScope`,
//! so its own reads are the accessors themselves and would move every time one
//! is refactored — a number that churns teaches people to bump it without
//! reading. It is still compiled in below, because
//! [`tests::the_accessors_this_guard_counts_still_exist`] has to check the
//! spellings this guard counts against the definitions they come from.
//!
//! `endpoint_tunnels.rs`, `settings.rs`, `agent_view.rs` and `appearance.rs`
//! read the applied selection nowhere, which is why they are absent rather than
//! present with a row of zeroes.
//!
//! Test modules are excluded by truncating each file at its trailing
//! `#[cfg(test)] mod tests {`, and **comment lines are stripped before
//! counting**. Both matter. Without the truncation a test fixture reads as a
//! production call site; without the comment stripping the guard counts prose —
//! these accessors are named dozens of times in doc comments, so the number
//! would track documentation edits instead of code, which is the substring-noise
//! trap `CLAUDE.md` rule 17 describes from the other direction.
//!
//! The guard normalizes CRLF to LF in every file it reads before scanning it.
//! The marker holds two line endings, and a CRLF checkout ends them with
//! `\r\n`, so against the raw text the guard would find no test module at all.

#[cfg(test)]
mod tests {
    /// Every raw read of the mutable applied selection this guard counts.
    ///
    /// `applied_selection()` is deliberately absent: it is private to `dto.rs`,
    /// so no consumer can call it and a column of zeroes would assert nothing.
    const ACCESSORS: [&str; 4] = [
        "selected_endpoint()",
        "selected_deck()",
        "observed_decks()",
        "deck_is_observed(",
    ];

    /// The consumer modules, compiled in so a moved or renamed file fails the
    /// build rather than silently scanning nothing.
    const MODULES: [(&str, &str); 4] = [
        ("lib.rs", include_str!("lib.rs")),
        ("daemon_bridge.rs", include_str!("daemon_bridge.rs")),
        ("terminal.rs", include_str!("terminal.rs")),
        ("endpoint_test.rs", include_str!("endpoint_test.rs")),
    ];

    /// How many reads of each [`ACCESSORS`] entry, in order, each module is
    /// allowed outside its tests.
    ///
    /// **Raising a number is a decision, not a chore.** Before you do, ask what
    /// the new reader is answering:
    ///
    /// - *"what is selected right now"* — a banner, a display set, a fresh
    ///   membership test taken because the decision is about the present. Fine;
    ///   raise the number and say why here.
    /// - *"which deck is my operation about"* — anything that will still be
    ///   using the answer after an `await`, cleanup very much included. Not
    ///   fine: capture a [`crate::dto::DeckScope`] once before the first await.
    ///
    /// Every allowance below is the first kind, and each is named:
    ///
    /// - `lib.rs` × 3 `selected_endpoint()`: `retarget_selection` reading what
    ///   was selected *before* this save (synchronous — no await between it and
    ///   the write it feeds), and the `StopDaemon` and `RestartDaemon` arms,
    ///   which each capture into a local once and reuse it across their awaits.
    ///   Those two are the shape `DeckScope` generalises; they were already
    ///   written this way and are left alone rather than churned.
    /// - `lib.rs` × 1 `observed_decks()`: `ensure_snapshot_watchers`, which
    ///   reads the set once and iterates it synchronously.
    /// - `endpoint_test.rs` × 1 `deck_is_observed(`: `release_if_not_observed`,
    ///   which **wants** the live answer — it decides whether something is
    ///   holding a transport *now*, and its doc comment says so. This is the
    ///   one site where a fresh read is the requirement rather than the bug.
    const BUDGET: [(&str, [usize; 4]); 4] = [
        ("lib.rs", [3, 0, 1, 0]),
        ("daemon_bridge.rs", [0, 0, 0, 0]),
        ("terminal.rs", [0, 0, 0, 0]),
        ("endpoint_test.rs", [0, 0, 0, 1]),
    ];

    /// Where every module in this crate keeps its tests, and therefore where
    /// the production half of each file ends.
    const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod tests {";

    /// `source` with every CRLF line ending turned into LF — the text this
    /// guard scans in every file it reads.
    ///
    /// `include_str!` embeds a file exactly as checked out, and a Windows
    /// checkout with `core.autocrlf` set — the GitHub runner's default — got
    /// CRLF. [`TEST_MODULE_MARKER`] holds two line endings, so against the raw
    /// text it matched nothing there and the guard failed on the first module
    /// it read with `found 0` — which is what `build-windows` did on PR #1126.
    /// The root `.gitattributes` now checks text out with LF on every platform,
    /// so this is defense in depth: a clone made before that attribute landed
    /// still has CRLF files until they are checked out again.
    fn lf(source: &str) -> String {
        source.replace("\r\n", "\n")
    }

    /// One module's production half with its comment lines removed — see the
    /// module docs for why both steps are load-bearing.
    fn production_code(name: &str, source: &str) -> String {
        let source = lf(source);
        let markers = source.matches(TEST_MODULE_MARKER).count();
        assert_eq!(
            markers, 1,
            "{name} must hold exactly one `#[cfg(test)] mod tests {{` for this guard to know \
             where its production half ends; found {markers}. A second test module would make \
             every count below an under-count, so this fails loudly rather than passing quietly."
        );
        let end = source
            .find(TEST_MODULE_MARKER)
            .expect("the marker was just counted");
        source[..end]
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// How many reads of each [`ACCESSORS`] entry, in order, one module's
    /// production half holds.
    fn raw_reads(name: &str, source: &str) -> [usize; 4] {
        let code = production_code(name, source);
        ACCESSORS.map(|accessor| code.matches(accessor).count())
    }

    /// Scenario: read the production code of every consumer module and count
    /// its raw reads of the applied selection. Each count must equal
    /// [`BUDGET`]'s exactly — so a new read fails here, and so does a stale
    /// allowance left behind by a reader that went away.
    ///
    /// This is the third identity audit's sweep, run as a test so it cannot go
    /// stale.
    #[test]
    fn no_module_grows_a_new_raw_read_of_the_applied_selection() {
        for (module, allowed) in BUDGET {
            let reads = MODULES
                .iter()
                .find(|(name, _)| *name == module)
                .map(|(name, source)| raw_reads(name, source))
                .unwrap_or_else(|| panic!("{module} must be in MODULES to be budgeted"));
            for ((accessor, found), allowed) in ACCESSORS.iter().zip(reads).zip(allowed) {
                assert_eq!(
                    found, allowed,
                    "{module} holds {found} raw `{accessor}` read(s) outside its tests, and \
                     the budget is {allowed}. A read whose answer is still used after an \
                     await belongs in a `crate::dto::DeckScope` captured once before it; \
                     only a read that genuinely means \"what is selected right now\" belongs \
                     here. See BUDGET's doc comment."
                );
            }
        }
    }

    /// A module shaped like a consumer: two production reads, an accessor
    /// named in a comment line, and a test module holding two more reads — so
    /// a count that ignored either the truncation or the comment stripping
    /// would differ from `[1, 0, 1, 0]`.
    const FIXTURE: &str = concat!(
        "fn operation() {\n",
        "    // selected_endpoint() named in prose is not a read\n",
        "    let endpoint = selected_endpoint();\n",
        "    let decks = observed_decks();\n",
        "}\n",
        "\n",
        "#[cfg(test)]\n",
        "mod tests {\n",
        "    fn fixture() {\n",
        "        selected_deck();\n",
        "        deck_is_observed(&deck);\n",
        "    }\n",
        "}\n",
    );

    /// Scenario: scan one module's text as an LF checkout gives it to
    /// `include_str!` and again as a Windows CRLF checkout does. Both must find
    /// exactly one test module and yield the same counts. The exactly-one
    /// check is [`production_code`]'s own assertion rather than a copy of it,
    /// so this exercises the code the guard runs.
    #[test]
    fn a_crlf_checkout_scans_the_same_as_an_lf_one() {
        let crlf = FIXTURE.replace('\n', "\r\n");
        assert!(
            !crlf.contains(TEST_MODULE_MARKER),
            "the raw CRLF fixture must NOT match the marker as written — if it does, it no \
             longer reproduces a Windows checkout and this test proves nothing"
        );

        let from_lf = raw_reads("lf.rs", FIXTURE);
        assert_eq!(from_lf, [1, 0, 1, 0], "the LF fixture's own counts");
        assert_eq!(
            raw_reads("crlf.rs", &crlf),
            from_lf,
            "line endings must not change what the guard counts"
        );
    }

    /// Scenario: give the scanner a CRLF module holding a second test module.
    /// It must still fail loudly with `found 2` — normalizing line endings
    /// must not relax "exactly one test module" into "at least one".
    #[test]
    #[should_panic(expected = "found 2")]
    fn a_crlf_checkout_with_a_second_test_module_still_fails_loudly() {
        let two_test_modules = format!("{FIXTURE}#[cfg(test)]\nmod tests {{\n}}\n");
        raw_reads("crlf.rs", &two_test_modules.replace('\n', "\r\n"));
    }

    /// Scenario: every module named in [`BUDGET`] is one [`MODULES`] can supply,
    /// and every module [`MODULES`] compiles in is one [`BUDGET`] has something
    /// to say about. Without this a typo in either table would leave the guard
    /// green while checking nothing — the failure mode a guard test can least
    /// afford, and the one that got this branch caught three times.
    #[test]
    fn the_budget_and_the_scanned_modules_name_the_same_files() {
        for (name, _) in MODULES {
            assert!(
                BUDGET.iter().any(|(module, _)| *module == name),
                "{name} is scanned but budgeted for nothing, so nothing is checked in it"
            );
        }
        for (module, _) in BUDGET {
            assert!(
                MODULES.iter().any(|(name, _)| *name == module),
                "{module} is budgeted but not scanned"
            );
        }
    }

    /// Scenario: the accessors this guard counts still exist in `dto.rs` under
    /// the spellings [`ACCESSORS`] uses. A rename would otherwise turn every
    /// budget line into a vacuous `0 == 0`: the guard would pass while counting
    /// a name nothing calls any more.
    #[test]
    fn the_accessors_this_guard_counts_still_exist() {
        let dto = lf(include_str!("dto.rs"));
        for signature in [
            "pub(crate) fn selected_endpoint()",
            "pub(crate) fn selected_deck()",
            "pub(crate) fn observed_decks()",
            "pub(crate) fn deck_is_observed(",
        ] {
            assert!(
                dto.contains(signature),
                "`{signature}` is gone from dto.rs, so the BUDGET column counting it now \
                 counts a name nothing calls — rename it in ACCESSORS or drop the column"
            );
        }
    }
}
