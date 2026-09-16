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

    /// One module's production half with its comment lines removed — see the
    /// module docs for why both steps are load-bearing.
    fn production_code(name: &str, source: &str) -> String {
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
            let source = MODULES
                .iter()
                .find(|(name, _)| *name == module)
                .map(|(name, source)| production_code(name, source))
                .unwrap_or_else(|| panic!("{module} must be in MODULES to be budgeted"));
            for (accessor, allowed) in ACCESSORS.iter().zip(allowed) {
                let found = source.matches(accessor).count();
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
        let dto = include_str!("dto.rs");
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
