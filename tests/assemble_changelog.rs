#![cfg(unix)]
//! Issue #582 — regression tests for `scripts/assemble-changelog.sh`, the
//! release-time fragment assembler.
//!
//! The script has two `find` invocations over `changelog.d/`: a validation
//! loop that rejects fragments whose type suffix it does not recognize, and a
//! collection loop that renders each fragment into `CHANGELOG.md` and then
//! `rm -f`s it. Those two loops disagreed on depth — validation was
//! `-maxdepth 1`, collection was unbounded — so anything below a subdirectory
//! was invisible to the guard and visible to the deletion. A fragment with an
//! unrecognized suffix could therefore sit in `changelog.d/nested/` while its
//! siblings were consumed and the release shipped without it: exactly the
//! v0.24.3 failure (`*.fix.md` silently ignored, empty release body) that the
//! guard was added to prevent, reached by the one path the guard did not cover.
//!
//! These are plain shell-level tests, NOT `#[spec]` catalog tests — the
//! subject is release tooling with no TUI surface, so there is no catalog
//! entry and no `/// Scenario:` comment (CLAUDE.md rule 7 binds `#[spec]`
//! tests only). They run in the fast tier: each one is a single `bash`
//! invocation against a scratch directory, no network, no sleeps.
//!
//! `#![cfg(unix)]`, which is where the subject can run. It was briefly
//! `#![cfg(target_os = "linux")]`: gated `unix` it turned `build-macos` red,
//! with every case failing identically at `assemble-changelog.sh: line 12:
//! added: unbound variable`. Line 12 was a `declare -A TYPE_HEADERS=(` block
//! that predated this file — macOS ships bash 3.2.57, which has no associative
//! arrays, so `[added]="Added"` was parsed as an *arithmetic* subscript and
//! `added` read as an unset variable under the script's `set -u`. The script
//! had therefore always required bash 4+, and these tests were simply the first
//! thing ever to execute it on a macOS runner. Narrowing to `linux` was the
//! right call for the #582 bugfix — a test that runs where the script does not
//! is a test whose red says nothing about the script's correctness — but it
//! parked the portability defect rather than fixing it. Issue #593 replaced the
//! associative array with a `case` function, so the script is now bash-3.2
//! clean and the gate is back to `unix`, which is what makes `build-macos`
//! prove that rather than leaving it resting on review.

// Issue #322 / linkage-check check 8: `tests/` may not call a bare `tempfile`
// constructor. This crate does not link the PTY harness, so it uses the
// self-contained resolver the same way `tests/features.rs` does.
#[path = "../src/test_temp.rs"]
mod test_temp;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/assemble-changelog.sh")
}

/// Run the assembler with `dir` as its working directory. The script resolves
/// `changelog.d/` and `CHANGELOG.md` relative to the cwd, so a scratch
/// directory is a complete stand-in for a checkout.
fn run(dir: &Path, version: &str) -> Output {
    Command::new("bash")
        .arg(script_path())
        .arg(version)
        .current_dir(dir)
        .output()
        .expect("bash runs and the assembler script is readable")
}

/// Write `changelog.d/<rel>` under `root`, creating intermediate directories.
fn write_fragment(root: &Path, rel: &str, body: &str) {
    let path = root.join("changelog.d").join(rel);
    fs::create_dir_all(path.parent().expect("fragment path has a parent"))
        .expect("scratch directory is writable");
    fs::write(path, body).expect("fragment is writable");
}

fn exists(root: &Path, rel: &str) -> bool {
    root.join("changelog.d").join(rel).exists()
}

/// The reported defect: a fragment nested in a subdirectory with an
/// unrecognized suffix reached neither the validation loop nor the release
/// notes, while its flat and nested siblings were rendered and deleted around
/// it. The guard must see the whole tree it deletes from, and must abort
/// before a single `rm -f` runs.
#[test]
fn nested_unknown_suffix_fragment_is_rejected_before_anything_is_deleted() {
    let scratch = test_temp::tempdir().expect("scratch dir");
    let root = scratch.path();

    write_fragment(root, ".gitkeep", "");
    write_fragment(root, "700.feature.md", "## A flat feature\n\nBody.\n");
    write_fragment(
        root,
        "nested/701.bugfix.md",
        "## A nested bugfix\n\nBody.\n",
    );
    // `fix` is not a recognized type (`bugfix`/`fixed` are) — the v0.24.3 typo.
    write_fragment(
        root,
        "nested/702.fix.md",
        "## A nested typo'd fix\n\nBody.\n",
    );

    let out = run(root, "9.9.9");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "an unrecognized suffix anywhere under changelog.d/ must abort the \
         release, but the script exited 0.\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        stderr.contains("nested/702.fix.md"),
        "the error must name the offender by a path the author can act on, \
         not by a basename that does not say which directory to look in.\n\
         stderr:\n{stderr}",
    );

    // The guard's whole purpose is to stop *before* the destructive half, so
    // nothing in the tree may have been consumed.
    assert!(
        exists(root, "700.feature.md"),
        "the flat fragment was deleted even though the run aborted",
    );
    assert!(
        exists(root, "nested/701.bugfix.md"),
        "the nested fragment was deleted without ever passing the guard — the \
         reported defect",
    );
    assert!(
        exists(root, "nested/702.fix.md"),
        "the offender was deleted"
    );
    assert!(
        !root.join("CHANGELOG.md").exists(),
        "a rejected run must not write a release section",
    );
}

/// Control for the test above: the same unrecognized fragment at the top level
/// is rejected, and always was. This is what pins the failure in the nested
/// case to *depth* rather than to the guard being broken in general — without
/// it, a green fix could just as well mean the guard started rejecting
/// everything.
#[test]
fn top_level_unknown_suffix_fragment_is_rejected() {
    let scratch = test_temp::tempdir().expect("scratch dir");
    let root = scratch.path();

    write_fragment(root, ".gitkeep", "");
    write_fragment(root, "702.fix.md", "## A typo'd fix\n\nBody.\n");

    let out = run(root, "9.9.9");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "stderr:\n{stderr}");
    assert!(stderr.contains("702.fix.md"), "stderr:\n{stderr}");
    assert!(exists(root, "702.fix.md"));
    assert!(!root.join("CHANGELOG.md").exists());
}

/// The flat happy path — the only layout the repo actually uses today — is
/// unchanged: recognized fragments are rendered under their mapped headings,
/// deleted afterwards, and `.gitkeep` survives.
#[test]
fn flat_fragments_are_assembled_and_then_deleted() {
    let scratch = test_temp::tempdir().expect("scratch dir");
    let root = scratch.path();

    write_fragment(root, ".gitkeep", "");
    write_fragment(
        root,
        "700.feature.md",
        "## A flat feature\n\nFeature body.\n",
    );
    write_fragment(root, "701.bugfix.md", "## A flat bugfix\n\nBugfix body.\n");

    let out = run(root, "9.9.9");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );

    let changelog = fs::read_to_string(root.join("CHANGELOG.md")).expect("CHANGELOG.md written");
    for haystack in [&stdout as &str, &changelog] {
        assert!(haystack.contains("## [9.9.9]"), "{haystack}");
        assert!(haystack.contains("### Added"), "{haystack}");
        assert!(haystack.contains("**A flat feature**"), "{haystack}");
        assert!(haystack.contains("### Fixed"), "{haystack}");
        assert!(haystack.contains("**A flat bugfix**"), "{haystack}");
    }

    assert!(
        !exists(root, "700.feature.md"),
        "processed fragment survived"
    );
    assert!(
        !exists(root, "701.bugfix.md"),
        "processed fragment survived"
    );
    assert!(exists(root, ".gitkeep"), ".gitkeep must never be consumed");
}

/// The chosen semantics for #582: the two loops converge on *recursive*, so a
/// nested fragment with a recognized suffix is validated, rendered into the
/// release notes, and only then deleted. Converging the other way — bounding
/// both loops to depth 1 — would leave it undeleted but also unmentioned and
/// unreported, which is the silent-drop shape the guard exists to prevent.
#[test]
fn nested_fragment_with_a_recognized_suffix_is_rendered_then_deleted() {
    let scratch = test_temp::tempdir().expect("scratch dir");
    let root = scratch.path();

    write_fragment(root, ".gitkeep", "");
    write_fragment(
        root,
        "nested/701.bugfix.md",
        "## A nested bugfix\n\nBody.\n",
    );

    let out = run(root, "9.9.9");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        stdout.contains("**A nested bugfix**"),
        "a validated nested fragment must reach the release notes rather than \
         being consumed silently.\nstdout:\n{stdout}",
    );
    assert!(!exists(root, "nested/701.bugfix.md"));
}

// ---------------------------------------------------------------------------
// Issue #593 — the type/heading mapping.
//
// The mapping used to be a `declare -A TYPE_HEADERS`, which is bash 4 and so
// died on macOS's bash 3.2.57 before the script did anything. It is now a
// `type_header()` `case`. That rewrite touched all nine arms while the tests
// above exercise only `feature` and `bugfix`, so the rest had no coverage.
//
// The mapping is declared in two places that must agree: the `TYPES` array
// (scan order) and `type_header()`'s arms (heading per type). Both lists are
// therefore READ OUT OF THE SCRIPT rather than mirrored here. A mirrored copy
// would have made these tests describe a lockstep they did not enforce — add a
// tenth type to the script and a hardcoded list simply never feeds it, so it
// goes untested while the test still claims to have checked it.

/// The script's `TYPES=(…)` array — the authoritative list, in scan order.
fn declared_types(script: &str) -> Vec<String> {
    let line = script
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("TYPES=("))
        .expect("the script declares a single-line TYPES=(…) array");
    line.trim_start_matches("TYPES=(")
        .split(')')
        .next()
        .expect("TYPES=(…) closes on its own line")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// `type_header()`'s `case` arms as `(aliases, heading)`, in source order. The
/// `*)` catch-all is the mismatch guard rather than a mapping, so it is left
/// out.
fn mapped_headings(script: &str) -> Vec<(Vec<String>, String)> {
    let body = script
        .split_once("type_header() {")
        .expect("the script defines type_header()")
        .1;
    let body = body
        .split_once("\n}")
        .expect("type_header() has a closing brace")
        .0;

    body.lines()
        .filter_map(|line| {
            let (labels, rest) = line.trim().split_once(')')?;
            // Arm labels are lowercase names joined by `|`. Anything else on a
            // line that happens to contain `)` — the `*)` catch-all, or the
            // error text mentioning `type_header()` — is not a mapping.
            if labels.is_empty() || !labels.chars().all(|c| c.is_ascii_lowercase() || c == '|') {
                return None;
            }
            let heading = rest
                .split('"')
                .nth(1)
                .expect("a mapping arm echoes a quoted heading");
            Some((
                labels.split('|').map(str::to_owned).collect(),
                heading.to_owned(),
            ))
        })
        .collect()
}

fn heading_for(map: &[(Vec<String>, String)], ty: &str) -> Option<String> {
    map.iter()
        .find(|(aliases, _)| aliases.iter().any(|a| a == ty))
        .map(|(_, heading)| heading.clone())
}

/// The lockstep itself, checked statically against the script text: every type
/// the script scans for has an arm that maps it to a heading, and no arm maps a
/// type the script never scans for.
///
/// A type in `TYPES` with no arm falls through to `*)` and aborts the release
/// — the failure this catches is a release going out short a section, and it
/// catches it without spawning a shell.
#[test]
fn every_declared_type_has_a_heading_arm_and_vice_versa() {
    let script = fs::read_to_string(script_path()).expect("the assembler script is readable");
    let declared = declared_types(&script);
    let map = mapped_headings(&script);

    assert!(
        !declared.is_empty() && !map.is_empty(),
        "the parsers found nothing — the script's shape changed and these \
         tests are no longer reading it.\nTYPES: {declared:?}\narms: {map:?}",
    );

    let unmapped: Vec<&String> = declared
        .iter()
        .filter(|ty| heading_for(&map, ty).is_none())
        .collect();
    assert!(
        unmapped.is_empty(),
        "these types are in TYPES but have no type_header() arm, so the \
         script aborts the moment a fragment of one shows up: {unmapped:?}",
    );

    let orphaned: Vec<&String> = map
        .iter()
        .flat_map(|(aliases, _)| aliases)
        .filter(|alias| !declared.contains(alias))
        .collect();
    assert!(
        orphaned.is_empty(),
        "these types have a type_header() arm but are not in TYPES, so no \
         fragment of them is ever scanned for: {orphaned:?}",
    );
}

/// One fragment of every type the script declares is rendered, under the
/// heading the script's own arms map it to, with aliases collapsing into a
/// single section.
///
/// Both the input types and the expected headings come from the script, so a
/// type added later is exercised automatically rather than silently skipped.
/// What is asserted is that the script's *runtime* behaviour matches its own
/// *declarations* — every fragment reaches the notes, each heading is opened
/// exactly once, and the sections come out in `TYPES` order.
#[test]
fn every_declared_type_renders_under_its_mapped_heading() {
    let script = fs::read_to_string(script_path()).expect("the assembler script is readable");
    let declared = declared_types(&script);
    let map = mapped_headings(&script);

    let scratch = test_temp::tempdir().expect("scratch dir");
    let root = scratch.path();
    write_fragment(root, ".gitkeep", "");
    for (i, ty) in declared.iter().enumerate() {
        write_fragment(
            root,
            &format!("{}.{ty}.md", 700 + i),
            &format!("## Entry for {ty}\n\nBody for {ty}.\n"),
        );
    }

    let out = run(root, "9.9.9");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "a fragment of every declared type must assemble cleanly.\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );

    for ty in &declared {
        assert!(
            stdout.contains(&format!("**Entry for {ty}**")),
            "the '{ty}' fragment never reached the release notes.\n\
             stdout:\n{stdout}",
        );
    }

    // First appearance of each mapped heading, walking TYPES in order — this
    // is what the aliasing is *for*, so `feature` must not open a section of
    // its own next to `added`.
    let mut expected: Vec<String> = Vec::new();
    for ty in &declared {
        let heading = format!(
            "### {}",
            heading_for(&map, ty).expect("checked by the lockstep test above"),
        );
        if !expected.contains(&heading) {
            expected.push(heading);
        }
    }

    let actual: Vec<&str> = stdout
        .lines()
        .map(str::trim_end)
        .filter(|l| l.starts_with("### "))
        .collect();
    assert_eq!(
        actual, expected,
        "the rendered sections must match what the script's own TYPES order \
         and type_header() arms imply — a repeat means the alias dedup broke, \
         a missing one means an arm stopped matching at runtime.\n\
         stdout:\n{stdout}",
    );
}
