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
fn declared_entries() -> Vec<String> {
    let source = std::fs::read_to_string(repo_root().join("src/daemon_protocol.rs"))
        .expect("src/daemon_protocol.rs is readable");
    let start = source
        .find("pub const CONTRACT_BREAKS: &[&str] = &[")
        .expect("`CONTRACT_BREAKS` is still declared in src/daemon_protocol.rs with that spelling");
    let body = &source[start..];
    let open = body.find('[').expect("the slice literal opens");
    let close = body[open..]
        .find("];")
        .expect("the slice literal closes with `];`");
    let list = &body[open + 1..open + close];
    list.split('"')
        // A quoted string literal is every ODD segment of a split on `"`, and
        // the comment lines between entries are the even ones.
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
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
        assert!(!slug.is_empty(), "`{entry}` must carry a slug");
    }
}
