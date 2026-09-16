//! Issue #801: a `changelog.d/<issue>.breaking.md` fragment and a
//! `CONTRACT_BREAKS` entry must move together.
//!
//! # What the pairing is for
//!
//! The desktop classifies daemon compatibility from
//! `daemon_protocol::CONTRACT_BREAKS` — the list of contract breaks a build
//! declares — instead of from the `git describe` stamp it used to read, because
//! a tag is applied after the content it names lands and so measures where a tag
//! sits in the commit graph rather than what the build can do.
//!
//! That only works while the list is *kept*. `docs/develop/versioning.md`
//! reserves the `breaking` changelog type for exactly one thing — a cross-process
//! TUI↔daemon contract break — so a fragment without an entry is a break the
//! classifier will not see, and its failure mode is silence: the handshake
//! connects, both sides report the same protocol version, and a field is read
//! with the wrong meaning. Nothing goes red on its own. This rule is what makes
//! it go red.
//!
//! # What it deliberately does not check
//!
//! **Entries without a fragment are fine**, and the rule is one-directional for
//! that reason: `release.yml` consumes a fragment into the changelog and deletes
//! it, while the entry stays for ever (the list is append-only). So every entry
//! older than the current release has no fragment left to pair with, and must
//! not be read as an error.
//!
//! **It cannot check that a break was declared at all.** An author who writes
//! neither the fragment nor the entry passes this rule, and passes every other
//! gate in the repo — which is the accepted residual named in `CONTRACT_BREAKS`'s
//! own doc comment, backstopped by CLAUDE.md rule 12's cross-version manual test.
//! What this closes is the narrower and much more likely miss: declaring the
//! break in the changelog and forgetting the half the software reads.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

/// The entries of `CONTRACT_BREAKS`, read out of the source rather than linked.
///
/// Parsed from text on purpose: `xtask/linkage-check` does not depend on the
/// root crate, and a text read also catches the constant being renamed or moved
/// out of the file the rule names — which a `use` would silently follow.
///
/// **Comments are stripped before the literals are read, and that is the
/// difference between a rule and a rule-shaped thing.** Splitting the body on
/// quotes alone treats every odd segment as an entry, so `// see "999-example"`
/// in the comment above a real entry would satisfy the check for
/// `999.breaking.md` with nothing in the runtime constant — and, worse, a single
/// unbalanced quote anywhere in a comment flips the parity and misreads every
/// entry after it. Both failures are silent and both are in the permissive
/// direction, which is the one this file exists to close.
///
/// Stripping `//` to end of line is safe for this constant specifically: an
/// entry is `<issue>-<kebab-slug>`, a charset with no `/` in it (asserted by
/// `the_entries_are_read_out_of_the_real_source` here and by
/// `declared_contract_breaks_are_well_formed_and_unique` in the constant's own
/// crate), so no string literal here can contain the sequence being stripped.
fn parse_entries(source: &str) -> Vec<String> {
    let start = source
        .find("pub const CONTRACT_BREAKS: &[&str] = &[")
        .expect("`CONTRACT_BREAKS` is still declared in src/daemon_protocol.rs with that spelling");
    let body = &source[start..];
    let open = body.find('[').expect("the slice literal opens");
    let close = body[open..]
        .find("];")
        .expect("the slice literal closes with `];`");
    let list = &body[open + 1..open + close];
    let code: String = list
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    code.split('"')
        // A quoted string literal is every ODD segment of a split on `"`; with
        // the comments gone, nothing else in this body can carry one.
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// [`parse_entries`] against the real file.
fn declared_entries() -> Vec<String> {
    let source = std::fs::read_to_string(repo_root().join("src/daemon_protocol.rs"))
        .expect("src/daemon_protocol.rs is readable");
    parse_entries(&source)
}

/// The issue numbers of the `*.breaking.md` fragments waiting in `changelog.d/`.
fn pending_breaking_fragments() -> Vec<String> {
    let dir = repo_root().join("changelog.d");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // No `changelog.d/` at all is not this rule's business to invent.
        return Vec::new();
    };
    let mut issues: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| name.strip_suffix(".breaking.md").map(str::to_string))
        .collect();
    issues.sort();
    issues
}

/// Every pending `.breaking.md` fragment has a `CONTRACT_BREAKS` entry.
#[test]
fn every_breaking_fragment_declares_a_contract_break() {
    let entries = declared_entries();
    for issue in pending_breaking_fragments() {
        let prefix = format!("{issue}-");
        assert!(
            entries.iter().any(|entry| entry.starts_with(&prefix)),
            "changelog.d/{issue}.breaking.md declares a TUI↔daemon contract break, but \
             `CONTRACT_BREAKS` in src/daemon_protocol.rs has no `{prefix}<slug>` entry. The \
             changelog fragment is what a human reads and the entry is what the desktop's \
             handshake reads; a break with only the first is a break the handshake connects \
             straight through. Declared entries: {entries:?}"
        );
    }
}

/// The parser reads the entries this rule is about, and reads *only* those.
///
/// Worth a test of its own because a parser that silently returns an empty list
/// would make the rule above vacuous — it would pass every fragment, for ever,
/// while reporting nothing. So this pins that the real file yields a non-empty,
/// well-formed list, which is the one property the rule cannot survive losing.
#[test]
fn the_entries_are_read_out_of_the_real_source() {
    let entries = declared_entries();
    assert!(
        !entries.is_empty(),
        "reading `CONTRACT_BREAKS` produced nothing, which would make the rule above vacuous"
    );
    for entry in &entries {
        let (issue, slug) = entry
            .split_once('-')
            .unwrap_or_else(|| panic!("`{entry}` must be `<issue>-<kebab-slug>`"));
        assert!(
            !issue.is_empty() && issue.bytes().all(|b| b.is_ascii_digit()),
            "`{entry}` must open with the issue number its fragment is named for"
        );
        // The charset is what makes stripping `//` before the split safe: no
        // entry can contain the sequence being stripped. `parse_entries`'s doc
        // comment cites this assertion, so the two must not drift.
        assert!(
            !slug.is_empty()
                && slug
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "`{entry}`'s slug must be lower-case kebab, with no `/` in it"
        );
    }
}

/// A quoted string in a COMMENT cannot satisfy the rule, and cannot derail the
/// entries that follow it either.
///
/// Both halves matter and only the first is obvious. A comment mentioning an
/// issue in quotes would have been read as a declaration — the rule passing on
/// prose while the runtime constant declares nothing. The second half is the
/// one that bites without anybody writing anything odd: an apostrophe-free but
/// quote-bearing comment shifts the parity of every split that follows, so the
/// real entries after it are read as the gaps between them. A permissive
/// misparse of a gate is a gate that reports success, which is the failure this
/// file exists to prevent.
#[test]
fn a_quoted_string_in_a_comment_is_not_a_declaration() {
    let source = r#"
pub const CONTRACT_BREAKS: &[&str] = &[
    // An issue mentioned in prose, as "999-not-a-declaration", declares nothing.
    "617-pane-write-agent-binding",
    // A lone quote in a comment " used to flip the parity of everything below.
    "704-orchestration-default",
];
"#;
    assert_eq!(
        parse_entries(source),
        vec![
            "617-pane-write-agent-binding".to_string(),
            "704-orchestration-default".to_string()
        ],
        "only the string literals are entries"
    );
}

/// The empty list parses as empty rather than as anything else — the shape the
/// constant takes the day a `PROTOCOL_VERSION` bump makes every entry moot.
#[test]
fn an_empty_constant_parses_as_no_entries() {
    let source = r#"
pub const CONTRACT_BREAKS: &[&str] = &[
    // Nothing declared yet.
];
"#;
    assert!(parse_entries(source).is_empty());
}
