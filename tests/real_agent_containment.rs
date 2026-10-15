//! Real-agent tests must not show their agents the developer's checkout
//! (PRD #1666).
//!
//! The deck spells its own absolute path into what agents read — every
//! delegated task's work-done footer, the orchestrator context, dispatch
//! prompts — and a real agent handed `<checkout>/target/debug/dot-agent-deck`
//! was measured taking the checkout for its project and writing its work there
//! (`orchestration/dispatch/002`). So every harness launch runs the deck from
//! `common::contained_deck_binary()`, a link outside the checkout, and puts it
//! on `PATH` through `common::contain_path`.
//!
//! These two tests keep that true as the suite grows. `orchestration/dispatch/007`
//! reads the lane-2 sources and refuses any route around the harness;
//! `orchestration/dispatch/008` checks the link and the `PATH` rewrite
//! themselves. `orchestration/dispatch/006` and `/009` check the launches.

mod common;

use std::path::{Path, PathBuf};

use spec::spec;

/// What a lane-2 source may not contain, and why.
const FORBIDDEN: &[(&str, &str)] = &[
    (
        "CARGO_BIN_EXE_dot-agent-deck",
        "cargo's path to the binary under test is inside the checkout; use \
         `common::contained_deck_binary()` for a binary an agent runs or reads, \
         `common::path_with_binary_dir()` for a PATH, or the harness fixtures",
    ),
    (
        "launching_the_cargo_artifact",
        "it runs the deck from cargo's build directory, and that deck names the \
         checkout in agent text",
    ),
];

/// The `tests/*.rs` files that hold a lane-2 test, by their own declaration
/// (the `e2e-live` feature in a file- or test-level `cfg`), plus every
/// `tests/support/` module one of them includes with `#[path = "support/…"]`.
fn lane_two_sources(tests: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(tests)
        .expect("list tests/")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter(|path| {
            std::fs::read_to_string(path).is_ok_and(|text| text.contains("feature = \"e2e-live\""))
        })
        .collect();
    let mut support: Vec<PathBuf> = files
        .iter()
        .flat_map(|path| {
            let text = std::fs::read_to_string(path).expect("read a lane-2 source");
            text.lines()
                .filter_map(|line| {
                    let rest = line.trim().strip_prefix("#[path = \"")?;
                    let (included, _) = rest.split_once('"')?;
                    Some(tests.join(included))
                })
                .collect::<Vec<_>>()
        })
        .collect();
    files.append(&mut support);
    files.sort();
    files.dedup();
    files
}

/// Scenario: Read every test source that holds a real-agent (lane-2) test and
/// every support module those sources include. None may name cargo's path to
/// the deck binary or opt out of the contained launch, so a new real-agent test
/// cannot start the deck, or hand an agent a deck path, from inside the checkout.
#[spec("orchestration/dispatch/007")]
#[test]
fn orchestration_dispatch_007_no_lane_two_source_routes_around_the_contained_deck() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let sources = lane_two_sources(&tests);
    // A scan that matched nothing would pass on any tree: the suite has dozens
    // of lane-2 files, and they include support modules.
    assert!(
        sources.len() >= 20,
        "found only {} lane-2 sources under {} — the scan's selector no longer \
         matches how lane-2 tests declare themselves: {sources:?}",
        sources.len(),
        tests.display()
    );
    assert!(
        sources
            .iter()
            .any(|path| path.starts_with(tests.join("support"))),
        "no support module was followed from a lane-2 file: {sources:?}"
    );

    let mut problems = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path).expect("read a lane-2 source");
        for (number, line) in text.lines().enumerate() {
            for (needle, why) in FORBIDDEN {
                if line.contains(needle) {
                    problems.push(format!(
                        "{}:{}: `{needle}` — {why}",
                        path.strip_prefix(&tests).unwrap_or(path).display(),
                        number + 1
                    ));
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "real-agent test code reaches the deck binary around the contained launch:\n{}",
        problems.join("\n")
    );
}

/// Scenario: Ask the harness for the binary every launch runs and for a contained
/// `PATH`. The binary must be the build under test at a path outside the
/// checkout, and the `PATH` must swap cargo's build directory for the contained
/// one in place, drop a checkout directory that resolves inside the checkout,
/// an outside symlink that resolves into it and every relative entry, keep
/// every other entry, and become the contained directory, never an empty
/// `PATH`, when nothing is left.
#[spec("orchestration/dispatch/008")]
#[test]
fn orchestration_dispatch_008_the_contained_link_and_path_keep_the_checkout_out() {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"));
    let canonical_checkout = checkout.canonicalize().expect("canonical checkout");
    let built = Path::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    let contained = common::contained_deck_binary();

    assert!(
        !contained.starts_with(checkout) && !contained.starts_with(&canonical_checkout),
        "the contained deck {} is inside the checkout {}",
        contained.display(),
        checkout.display()
    );
    assert_eq!(
        std::fs::read(contained).expect("read the contained deck"),
        std::fs::read(built).expect("read the built deck"),
        "the contained deck must be the build under test, byte for byte"
    );
    // A symlink on unix, so it resolves to the link; elsewhere a second hard link.
    let on_path = common::contained_deck_path_dir().join(contained.file_name().unwrap());
    #[cfg(unix)]
    assert_eq!(
        on_path.canonicalize().expect("the PATH entry resolves"),
        contained
            .canonicalize()
            .expect("the contained deck resolves"),
        "a lookup by name through the contained PATH directory must reach the contained deck"
    );
    assert_ne!(
        common::contained_deck_path_dir(),
        contained.parent().unwrap(),
        "the PATH directory must not be the link's own directory, or the resolver \
         treats the running deck as installed and pins that path into agent config"
    );

    let cargo_dir = built.parent().unwrap();
    let inside = checkout.join("tests");
    let outside = std::env::temp_dir();
    let path = std::env::join_paths([cargo_dir, &inside, &outside, cargo_dir])
        .expect("join a synthetic PATH");
    let rewritten = common::contain_path(&path.to_string_lossy());
    let entries: Vec<PathBuf> = std::env::split_paths(&rewritten).collect();
    assert_eq!(
        entries,
        vec![
            common::contained_deck_path_dir().to_path_buf(),
            outside.clone()
        ],
        "cargo's directory becomes the contained one (once), a checkout directory \
         with no outside spelling is dropped, the rest is kept: {rewritten}"
    );

    // A relative entry is resolved against whatever directory the child runs
    // in, which for an agent can be the checkout, so it is dropped — the empty
    // entry (the current directory) included.
    let relative = std::env::join_paths([
        Path::new("tests"),
        Path::new("."),
        Path::new(""),
        Path::new("target/debug"),
        outside.as_path(),
    ])
    .expect("join a PATH with relative entries");
    let rewritten = common::contain_path(&relative.to_string_lossy());
    assert_eq!(
        std::env::split_paths(&rewritten).collect::<Vec<_>>(),
        vec![outside.clone()],
        "every relative entry is dropped and the absolute one kept: {rewritten}"
    );

    // When every entry is dropped the result is not the empty string: an
    // empty `PATH` on unix searches the child's current directory, the very
    // lookup dropping relative entries took away. It is the contained deck's
    // directory instead, an absolute one holding nothing but the build under
    // test.
    let all_dropped = std::env::join_paths([inside.as_path(), Path::new("."), Path::new("bin")])
        .expect("join a PATH with nothing to keep");
    for path in [".".to_string(), all_dropped.to_string_lossy().into_owned()] {
        let rewritten = common::contain_path(&path);
        assert_eq!(
            std::env::split_paths(&rewritten).collect::<Vec<_>>(),
            vec![common::contained_deck_path_dir().to_path_buf()],
            "{path:?} keeps no entry, so it becomes the contained deck's directory, \
             never an empty PATH: {rewritten:?}"
        );
    }

    // An entry OUTSIDE the checkout that is a symlink into it — a
    // `~/bin/project-tools` pointing at `<checkout>/scripts` — reaches the
    // checkout as surely as its inside spelling, so it is dropped too, and so
    // is a path below it.
    #[cfg(unix)]
    {
        let aliases = tempfile::Builder::new()
            .prefix("contain-path-alias-")
            .tempdir_in(&outside)
            .expect("scratch dir outside the checkout");
        assert!(
            !aliases.path().starts_with(checkout)
                && !aliases.path().starts_with(&canonical_checkout),
            "the system temp dir must lie outside the checkout for this case"
        );
        let alias = aliases.path().join("project-tools");
        std::os::unix::fs::symlink(&inside, &alias).expect("link an outside alias in");
        let below = alias.join("fixtures");
        let missing = alias.join("no-such-dir");
        let path = std::env::join_paths([alias.as_path(), &below, &missing, outside.as_path()])
            .expect("join a PATH with aliases");
        let rewritten = common::contain_path(&path.to_string_lossy());
        assert_eq!(
            std::env::split_paths(&rewritten).collect::<Vec<_>>(),
            vec![outside.clone()],
            "an outside symlink into the checkout, and a path below it, is dropped: {rewritten}"
        );
    }

    // An entry in the checkout that is a symlink to somewhere else keeps
    // working under its outside spelling — a devbox shell's
    // `<checkout>/.devbox/nix/profile/default/bin` is one. Built under cargo's
    // per-target scratch directory, which is inside the checkout exactly when the
    // target directory is; with a target directory elsewhere there is no way to
    // build one without writing into the checkout, so that half is skipped.
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"));
    if scratch.starts_with(checkout) || scratch.starts_with(&canonical_checkout) {
        let dir = tempfile::Builder::new()
            .prefix("contain-path-")
            .tempdir_in(scratch)
            .expect("scratch dir under the target directory");
        let profile = dir.path().join("profile-bin");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &profile).expect("link the profile dir out");
        #[cfg(unix)]
        {
            let rewritten = common::contain_path(&profile.to_string_lossy());
            assert_eq!(
                std::env::split_paths(&rewritten).collect::<Vec<_>>(),
                vec![outside.canonicalize().expect("canonical temp dir")],
                "a checkout entry that resolves outside is kept as its canonical path"
            );
        }
    }
}
