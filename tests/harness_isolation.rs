//! The test harness must detach from any real deck before it spawns anything.
//!
//! Running the suite from inside a deck pane means this process inherits that
//! pane's `DOT_AGENT_DECK_SOCKET` / `_PANE_ID`. Anything spawned inherits them
//! too, and its hooks then post into the developer's LIVE dashboard — a card
//! appears under a fixture pane id and vanishes again. `ff5170d` scrubs these in
//! `agent_pty::spawn`, which is necessary but not sufficient: real `deck.log`
//! evidence shows four of five leaked fixture pane ids arriving from a tree that
//! already had that fix, through other spawn paths. Clearing the vars from the
//! test process covers every spawn path at once, including ones added later.
//!
//! nextest runs each test in its own process, so mutating this process's
//! environment cannot affect another test.

mod common;

/// Scenario: Set all four deck endpoint variables to values that mimic a live
/// deck, call the harness setup hook, and assert every one of them is gone —
/// so no child this process spawns can inherit a route to a real daemon.
#[test]
fn harness_clears_inherited_deck_endpoints() {
    for (var, value) in [
        (
            "DOT_AGENT_DECK_SOCKET",
            "/run/user/1000/dot-agent-deck.sock",
        ),
        (
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            "/run/user/1000/dot-agent-deck-attach.sock",
        ),
        ("DOT_AGENT_DECK_PANE_ID", "8"),
        ("DOT_AGENT_DECK_AGENT_ID", "8"),
    ] {
        // SAFETY: single-threaded test body, before the harness starts anything.
        unsafe { std::env::set_var(var, value) };
    }

    common::init_test_env();

    for var in [
        "DOT_AGENT_DECK_SOCKET",
        "DOT_AGENT_DECK_ATTACH_SOCKET",
        "DOT_AGENT_DECK_PANE_ID",
        "DOT_AGENT_DECK_AGENT_ID",
    ] {
        assert!(
            std::env::var_os(var).is_none(),
            "{var} survived harness setup — a spawned child would inherit it and \
             could post hook events into a live deck"
        );
    }
}

// ---------------------------------------------------------------------------
// PR #805 audit blocker 3 — a stale recording must not survive a run
// ---------------------------------------------------------------------------
//
// `TuiDeck` writes its artifacts only from `Drop`, and only on a panic or under
// `DOT_AGENT_DECK_RECORD`. Every route that ends a run without reaching `Drop` —
// SIGKILL, a nextest timeout, Ctrl-C, a runtime skip before launch, a failed
// re-recording — used to leave the PREVIOUS run's `full-stream.cast` in place,
// and `.claude/skills/demo-reel-adapter` selects a cast on path existence alone.
// A cast from a revision predating the redaction fixes could therefore be
// stitched into a video and uploaded to YouTube with its link in the PR body and
// the public release notes. (The upload is private by default now, so widening
// it beyond the channel owner and the accounts they deliberately share it with
// is a human step — that bounds the blast radius; it does not make a stale cast
// correct.)
//
// Both call sites of the discard are covered here: the runtime-skip one by
// actually taking it, and the launch one by a source guard, because observing it
// needs a real PTY launch and this file is in the fast tier.
//
// Issue #808 added the other half, and the guards for it live below the discard
// ones in this file. Clearing is keyed to REACHING one of those two call sites,
// and two routes do not: a FILTERED run never selects the test at all, and
// `skip_unless!` evaluates its preflight before `_skip_if_err` is entered. The
// answer to both is that the adapter checks PROVENANCE rather than existence, so
// what has to hold here is that the harness writes a sidecar the adapter can
// read, and that the two ends of that contract cannot drift apart silently.

/// The artifacts the harness dumps — the set the discard has to clear. Mirrors
/// `RECORDING_ARTIFACTS` in `tests/common/mod.rs`; the guard below proves the two
/// lists and the dump itself still agree.
const RECORDING_ARTIFACTS: [&str; 5] = [
    "provenance.json",
    "final-grid.txt",
    "final-grid.svg",
    "full-stream.cast",
    "fixture.toml",
];

fn adapter_build_script() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".claude")
        .join("skills")
        .join("demo-reel-adapter")
        .join("build.sh");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn harness_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("common")
        .join("mod.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Scenario: Plant a stale `full-stream.cast` — plus the other three recording
/// artifacts and the paired `test.md` — in this test's own recordings directory,
/// then take the runtime-skip path through `skip_unless!`'s helper, which is one
/// of the routes that used to leave that cast behind while nextest reported the
/// test as passed. Afterwards every recording artifact is gone — including the
/// `provenance.json` sidecar that would otherwise still vouch for the stale cast
/// — and the paired doc is untouched.
#[test]
fn a_runtime_skip_discards_the_previous_recording() {
    // The helper PANICS instead of skipping when this is set, which would make
    // the test's outcome depend on the developer's environment.
    // SAFETY: single-threaded test body; nextest gives each test its own process.
    unsafe { std::env::remove_var("DOT_AGENT_DECK_REQUIRE_REAL_E2E") };

    let dir = common::current_test_recordings_dir();
    std::fs::create_dir_all(&dir).expect("create this test's recordings dir");
    for name in RECORDING_ARTIFACTS {
        std::fs::write(dir.join(name), b"STALE-ARTIFACT-FROM-AN-EARLIER-REVISION")
            .expect("plant a stale artifact");
    }
    std::fs::write(dir.join("test.md"), b"# generated from the test source\n")
        .expect("plant the paired doc");

    let skipped = common::_skip_if_err(Err("no credential on this host".to_string()));
    assert!(
        skipped,
        "an Err must produce a skip, or this proves nothing"
    );

    for name in RECORDING_ARTIFACTS {
        let path = dir.join(name);
        assert!(
            !path.exists(),
            "{} survived a runtime skip, so an interrupted or skipped run can \
             still hand the demo reel an artifact it did not produce",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("test.md")).expect("the paired doc must survive"),
        "# generated from the test source\n",
        "the paired `.md` is regenerated from the test source, carries no \
         credential and no run identity, and must not be deleted with the \
         artifacts"
    );

    // This test's recordings directory is its own fixture, so it takes it away
    // again — but only on success, so a failure leaves the evidence in place.
    std::fs::remove_dir_all(&dir).expect("remove this test's fixture recordings dir");
}

/// Scenario: Plant a stale recording artifact the discard CANNOT delete — a
/// directory where `full-stream.cast` should be — and take the runtime-skip path.
/// The harness panics instead of warning and carrying on, so the test fails
/// rather than running to a green finish with an artifact it did not produce.
///
/// PR #805's second audit named the old warn-and-continue a fail-open and it was
/// right: a warning only helps if somebody reads it, the run that printed it was
/// still reported as PASSED, and the artifact it could not remove still satisfies
/// the demo-reel adapter's existence check. Unix-only, because "a deletion that
/// fails for a reason other than NotFound" is arranged here through `EISDIR`.
#[cfg(unix)]
#[test]
fn a_discard_that_cannot_delete_a_stale_artifact_fails_the_run() {
    // SAFETY: single-threaded test body; nextest gives each test its own process.
    unsafe { std::env::remove_var("DOT_AGENT_DECK_REQUIRE_REAL_E2E") };

    let dir = common::current_test_recordings_dir();
    std::fs::create_dir_all(&dir).expect("create this test's recordings dir");
    // `remove_file` on a directory is EISDIR, not NotFound — an undeletable
    // stale artifact, without having to make the tree unwritable (which would
    // also stop the harness from cleaning up after itself).
    let undeletable = dir.join("full-stream.cast");
    std::fs::create_dir_all(&undeletable).expect("plant an undeletable stale artifact");

    let outcome =
        std::panic::catch_unwind(|| common::_skip_if_err(Err("no credential".to_string())));

    // Cleaned up before the assertions, so a failure does not leave a directory
    // named `full-stream.cast` behind to break every later run of this test.
    std::fs::remove_dir_all(&dir).expect("remove this test's fixture recordings dir");

    let payload = outcome.expect_err(
        "the discard must FAIL the run when it cannot remove a stale artifact — a \
         warning leaves the run green with a recording it did not produce, which \
         is exactly what the demo-reel adapter then publishes",
    );
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
        .unwrap_or_default();
    assert!(
        message.contains("could not discard the previous recording")
            && message.contains("full-stream.cast"),
        "the panic must name the artifact it could not remove: {message}"
    );
}

/// Scenario: Read the harness source and assert that `TuiDeck::try_launch_inner`
/// discards the previous recording, that it does so before it spawns anything,
/// and that the artifact list the discard walks still names every file
/// `dump_recordings` writes.
///
/// A source guard rather than an observation, because observing it needs a real
/// PTY launch and this file is in the fast tier — the launch path itself is
/// exercised by the 57 of the 71 `tests/e2e_*.rs` files that build a `TuiDeck`
/// (the other 14 drive a headless or in-process daemon and never launch one).
#[test]
fn tui_deck_launch_discards_the_previous_recording_before_it_spawns() {
    let source = harness_source();
    let body = source
        .split_once("fn try_launch_inner(")
        .expect("try_launch_inner must exist")
        .1;

    let discard = body.find("discard_previous_recording(&test_name)").expect(
        "try_launch_inner no longer discards the previous recording — an \
             interrupted run can leave a stale cast for the demo reel to \
             publish (PR #805 audit blocker 3)",
    );
    let spawn = body
        .find("slave.spawn_command")
        .or_else(|| body.find(".spawn_command("))
        .expect("try_launch_inner must spawn the deck somewhere");
    assert!(
        discard < spawn,
        "the discard must run BEFORE the deck is spawned: from the spawn onwards \
         the run can be killed at any instant, and whatever it has not \
         overwritten is what a later reel build picks up"
    );

    // The dump and the discard must agree on the file names, or the discard
    // silently stops covering whatever the dump added.
    let dump = source
        .split_once("fn dump_recordings(")
        .expect("dump_recordings must exist")
        .1;
    let dump = dump.split_once("\n    fn ").map_or(dump, |(head, _)| head);
    for name in RECORDING_ARTIFACTS {
        assert!(
            source.contains(&format!("\"{name}\",")),
            "{name} is missing from RECORDING_ARTIFACTS in tests/common/mod.rs"
        );
    }
    let mut written: Vec<String> = Vec::new();
    let mut rest = dump;
    while let Some((_, after)) = rest.split_once("dir.join(\"") {
        let (name, tail) = after
            .split_once('"')
            .expect("unterminated dir.join literal");
        written.push(name.to_string());
        rest = tail;
    }
    assert!(
        !written.is_empty(),
        "found no `dir.join(\"…\")` artifact writes in dump_recordings — this \
         guard has stopped guarding anything"
    );
    for name in &written {
        assert!(
            RECORDING_ARTIFACTS.contains(&name.as_str()),
            "dump_recordings writes `{name}`, which the launch-time discard does \
             not remove — add it to RECORDING_ARTIFACTS in tests/common/mod.rs, \
             or an interrupted run leaves it behind (PR #805 audit blocker 3)"
        );
    }
}

// ---------------------------------------------------------------------------
// Issue #808 — the cast provenance sidecar
// ---------------------------------------------------------------------------

/// Scenario: Feed `recording_build_commit` every `DAD_BUILD_ID` shape
/// `build.rs` can compose plus the ones an operator can inject, and assert it
/// extracts the commit and the dirty flag from the real ones while yielding no
/// commit for anything that cannot identify a revision.
///
/// The commit in `provenance.json` is a publish gate, so a parse that returns a
/// plausible-looking non-commit is worse than one that returns nothing: an empty
/// `commit` makes the adapter refuse the clip, which is the safe direction.
#[test]
fn the_build_id_parse_yields_a_commit_only_when_one_is_really_there() {
    for (build_id, want_commit, want_dirty) in [
        // The ordinary shapes `resolve_build_id` composes.
        ("0.39.2-g5a56361", Some("5a56361"), false),
        ("0.39.2-g5a56361-dirty", Some("5a56361"), true),
        // A SemVer prerelease keeps its own hyphens, and one of them can begin
        // with `g` — which is why the sha is taken from the LAST `-g` and not
        // the first.
        ("0.25.0-gamma.1-g5a56361", Some("5a56361"), false),
        ("0.25.0-gamma.1-g5a56361-dirty", Some("5a56361"), true),
        // Longer abbreviations are fine; the floor is a minimum, not a length.
        ("0.39.2-gdeadbeefcafe1234", Some("deadbeefcafe1234"), false),
        // The `-unknown` sentinel a git-less or shallow build composes. No
        // commit, so the adapter refuses anything recorded by such a build.
        ("0.39.2-unknown", None, false),
        // A prerelease that merely LOOKS like it carries a sha.
        ("0.1.0-gamma", None, false),
        // An injected DAD_BUILD_ID (issue #250) that says nothing about a
        // revision, with and without the operator adding the dirty suffix.
        ("ci-build-1234", None, false),
        ("ci-build-1234-dirty", None, true),
        // Too short to be evidence: git's auto-abbreviation floor is 7, so a
        // 6-character prefix is a `core.abbrev` setting, not a commit.
        ("0.39.2-gabc123", None, false),
        // Not hex, so not a sha however long it is.
        ("0.39.2-gnothexatall", None, false),
    ] {
        let (commit, dirty) = common::recording_build_commit(build_id);
        assert_eq!(
            commit, want_commit,
            "recording_build_commit({build_id:?}) extracted the wrong commit"
        );
        assert_eq!(
            dirty, want_dirty,
            "recording_build_commit({build_id:?}) got the dirty flag wrong"
        );
    }
}

/// Scenario: Read the harness source and assert the provenance sidecar is
/// written LAST of the dump's artifacts while sitting FIRST in the list the
/// launch-time discard walks.
///
/// Both orders are fail-closed and they point opposite ways, which is why
/// neither is an accident worth leaving unguarded. Written last: a dump that
/// dies partway leaves a cast with no sidecar, and the adapter refuses a cast
/// with no sidecar — so a torn dump produces nothing publishable rather than
/// something publishable. Discarded first: a discard that panics partway has
/// already removed the sidecar that would have vouched for whatever survives.
#[test]
fn the_provenance_sidecar_is_written_last_and_discarded_first() {
    let source = harness_source();

    let dump = source
        .split_once("fn dump_recordings(")
        .expect("dump_recordings must exist")
        .1;
    let dump_body = dump.split_once("\n    /// ").map_or(dump, |(head, _)| head);
    let cast = dump_body
        .find("full-stream.cast")
        .expect("dump_recordings must write the cast");
    let provenance = dump_body.find("write_provenance(").expect(
        "dump_recordings no longer writes the provenance sidecar — the demo-reel \
         adapter then has nothing to check and falls back to publishing on path \
         existence (issue #808)",
    );
    assert!(
        cast < provenance,
        "the provenance sidecar must be written AFTER the cast it vouches for: a \
         dump that dies in between must leave a cast the adapter refuses, not one \
         it accepts"
    );

    let list = source
        .split_once("const RECORDING_ARTIFACTS")
        .expect("RECORDING_ARTIFACTS must exist")
        .1;
    let list = list.split_once("];").expect("unterminated array").0;
    let prov_pos = list
        .find("\"provenance.json\"")
        .expect("provenance.json must be in RECORDING_ARTIFACTS, or an interrupted run leaves a sidecar vouching for a cast it did not produce");
    let cast_pos = list
        .find("\"full-stream.cast\"")
        .expect("full-stream.cast must be in RECORDING_ARTIFACTS");
    assert!(
        prov_pos < cast_pos,
        "provenance.json must be discarded BEFORE full-stream.cast: the discard \
         panics on the first failure, so removing the sidecar first means a \
         partial discard leaves nothing publishable"
    );
}

/// Scenario: Read every field the harness's `write_provenance` puts in
/// `provenance.json` and every field the adapter's `build.sh` reads out of it,
/// then assert the adapter's read set is a subset of the harness's write set and
/// that both sides agree on the schema number.
///
/// The two halves of this contract live in different languages and neither
/// compiles the other, so drift is invisible until a reel build refuses every
/// clip. That failure is at least safe — an unreadable field reads as absent and
/// the adapter refuses — but it is also silent until somebody tries to publish,
/// which for a lane-2-only artifact can be weeks. Cheap to check here instead.
///
/// Deliberately one-directional: a field the harness writes and the adapter
/// ignores (`test`, the recording's own name) breaks nothing, while a field the
/// adapter requires and the harness stopped writing breaks everything.
#[test]
fn the_provenance_contract_the_adapter_reads_is_the_one_the_harness_writes() {
    let source = harness_source();
    let script = adapter_build_script();

    let literal = source
        .split_once("let provenance = serde_json::json!({")
        .expect("write_provenance must build the sidecar with a json! literal")
        .1;
    let literal = literal
        .split_once("});")
        .expect("unterminated json! literal")
        .0;
    let mut written: Vec<String> = Vec::new();
    let mut rest = literal;
    while let Some((_, after)) = rest.split_once('"') {
        let (name, tail) = after.split_once('"').expect("unterminated field name");
        written.push(name.to_string());
        rest = tail;
    }
    assert!(
        written.len() >= 8,
        "found only {} field names in the provenance literal — this guard has \
         stopped guarding anything: {written:?}",
        written.len()
    );

    // The adapter's read set, taken from the one jq filter that destructures the
    // sidecar: each field appears there as `(.<name> // …)`.
    let filter = script
        .split_once("if ! fields=\"$(jq -r '")
        .expect("build.sh must read the sidecar through a single jq filter")
        .1;
    let filter = filter
        .split_once("' \"$file\"")
        .expect("unterminated jq filter")
        .0;
    let mut read: Vec<String> = Vec::new();
    let mut rest = filter;
    while let Some((_, after)) = rest.split_once("(.") {
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() && after[name.len()..].starts_with(" //") {
            read.push(name);
        }
        rest = after;
    }
    assert!(
        read.len() >= 8,
        "found only {} fields in the adapter's jq filter — this guard has stopped \
         guarding anything: {read:?}",
        read.len()
    );

    for field in &read {
        assert!(
            written.iter().any(|w| w == field),
            "`.claude/skills/demo-reel-adapter/build.sh` reads provenance field \
             `{field}`, which `write_provenance` in tests/common/mod.rs does not \
             write. The adapter treats an absent field as a refusal, so the reel \
             would silently stop selecting every clip. Write the field, or stop \
             reading it. (harness writes: {written:?})"
        );
    }

    // The schema number gates the whole sidecar, so a bump on one side alone
    // refuses every clip.
    let harness_schema = source
        .split_once("const RECORDING_PROVENANCE_SCHEMA: u32 = ")
        .expect("RECORDING_PROVENANCE_SCHEMA must exist")
        .1
        .split_once(';')
        .expect("unterminated const")
        .0
        .trim()
        .to_string();
    let adapter_schema = script
        .split_once("\nPROVENANCE_SCHEMA=")
        .expect("build.sh must declare PROVENANCE_SCHEMA")
        .1
        .lines()
        .next()
        .expect("PROVENANCE_SCHEMA has no value")
        .trim()
        .to_string();
    assert_eq!(
        harness_schema, adapter_schema,
        "the harness writes provenance schema {harness_schema} and the adapter \
         only accepts {adapter_schema} — every clip would be refused. Bump both \
         in the same commit."
    );
}
