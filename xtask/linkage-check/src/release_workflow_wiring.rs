//! PRD #740: the desktop GUI's alpha bundles are published by two jobs added to
//! `.github/workflows/release.yml`, and the whole reason they are safe to run
//! on every tag is a property of the job graph rather than of the code inside
//! them:
//!
//! - `desktop-bundle` hangs off `prepare`, **not** `build`, so it runs
//!   concurrently with the CLI matrix and adds nothing to the critical path;
//! - `finalize` — the job that creates the GitHub Release and uploads the five
//!   CLI assets — does **not** list either desktop job in `needs:`, so a
//!   bundler that fails, hangs or is skipped cannot delay or break an ordinary
//!   release;
//! - `finalize`'s artifact download is constrained by `pattern:`, because
//!   `merge-multiple: true` otherwise flattens every artifact of the run into
//!   one directory. Whether a desktop bundle appeared in `dist/` would then
//!   depend on which job finished first, and Tauri's `Agent Deck_<v>_amd64.deb`
//!   output name — with a space — would be swept into the release glob and fed
//!   to `task checksums`' `shasum -a 256 dot-agent-deck-*`.
//!
//! Every one of those is a single line that reads as housekeeping and is not.
//! Nothing else checks them: no test can run this workflow, `release.yml` fires
//! only on a tag, and by the time a bad edit is observable a release has
//! already gone out wrong. So these are static assertions over the file — a
//! narrow claim, deliberately: they prove the wiring is what PRD #740 decided,
//! not that a bundle builds.
//!
//! Parsed by indentation rather than with a YAML crate, because the workspace
//! has no YAML dependency and adding one to assert five lines is a poor trade.
//! The parse is strict about what it does not understand: an unrecognised job
//! header or a missing job fails loudly instead of vacuously passing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn workflow() -> String {
    let path = repo_root().join(".github/workflows/release.yml");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Split the `jobs:` mapping into `name -> block`, where a block runs from the
/// job's own header to the next 2-space-indented header.
fn jobs(text: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<(String, Vec<String>)> = None;
    let mut in_jobs = false;

    for line in text.lines() {
        if line == "jobs:" {
            in_jobs = true;
            continue;
        }
        if !in_jobs {
            continue;
        }
        let is_job_header = line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
            && !line.trim_start().starts_with('#');
        if is_job_header {
            if let Some((name, body)) = current.take() {
                out.insert(name, body.join("\n"));
            }
            let name = line.trim().trim_end_matches(':').to_string();
            current = Some((name, Vec::new()));
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push(line.to_string());
        }
    }
    if let Some((name, body)) = current.take() {
        out.insert(name, body.join("\n"));
    }
    out
}

fn job<'a>(all: &'a BTreeMap<String, String>, name: &str) -> &'a str {
    all.get(name).unwrap_or_else(|| {
        panic!(
            "release.yml has no `{name}` job; found {:?}. If a job was renamed, \
             this check needs updating rather than deleting — it guards the \
             property that a desktop bundler cannot break a CLI release.",
            all.keys().collect::<Vec<_>>()
        )
    })
}

/// The `needs:` line of a job block, normalised to a list of job names.
fn needs(block: &str) -> Vec<String> {
    let line = block
        .lines()
        .find(|l| l.trim_start().starts_with("needs:"))
        .unwrap_or_else(|| panic!("job block has no `needs:` line:\n{block}"));
    let value = line.split_once("needs:").expect("needs:").1.trim();
    value
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn finalize_does_not_wait_on_the_desktop_jobs() {
    let all = jobs(&workflow());
    let finalize = needs(job(&all, "finalize"));

    for forbidden in ["desktop-bundle", "desktop-publish"] {
        assert!(
            !finalize.iter().any(|n| n == forbidden),
            "`finalize` lists `{forbidden}` in needs: {finalize:?}.\n\
             This is the single property that keeps a desktop bundler failure \
             off the CLI release: `finalize` creates the GitHub Release and \
             uploads the five CLI assets, so making it wait on a Tauri bundle \
             means a broken bundler delays — or blocks — an ordinary tag. PRD \
             #740 Decision 4."
        );
    }
    assert_eq!(
        finalize,
        vec!["prepare".to_string(), "build".to_string()],
        "`finalize` should still depend on exactly prepare and build"
    );
}

#[test]
fn desktop_bundle_runs_beside_the_cli_matrix_not_after_it() {
    let all = jobs(&workflow());
    let bundle = needs(job(&all, "desktop-bundle"));

    assert_eq!(
        bundle,
        vec!["prepare".to_string()],
        "`desktop-bundle` must depend on `prepare` alone, got {bundle:?}. \
         Depending on `build` would serialize the bundle behind the CLI matrix \
         and put it on the critical path, which is exactly what PRD #740 \
         Decision 4's topology exists to avoid."
    );
}

#[test]
fn desktop_publish_waits_for_the_release_to_exist() {
    let all = jobs(&workflow());
    let publish = needs(job(&all, "desktop-publish"));

    assert!(
        publish.iter().any(|n| n == "finalize"),
        "`desktop-publish` must wait on `finalize`, got {publish:?}: it uploads \
         to a release object that `finalize` creates, so without this it races \
         a release that may not exist yet."
    );
    assert!(
        publish.iter().any(|n| n == "desktop-bundle"),
        "`desktop-publish` must wait on `desktop-bundle`, got {publish:?}"
    );
}

#[test]
fn finalize_downloads_only_the_cli_artifacts() {
    let all = jobs(&workflow());
    let finalize = job(&all, "finalize");

    assert!(
        finalize.contains("pattern: dot-agent-deck-*"),
        "`finalize`'s download-artifact step must carry \
         `pattern: dot-agent-deck-*`.\n\
         `merge-multiple: true` flattens every artifact of the run into one \
         directory, so without a pattern whether a desktop bundle lands in \
         `dist/` depends on which job finished first. It would then be swept \
         into the release by the `dist/dot-agent-deck-*` glob and handed to \
         `task checksums`, whose `shasum -a 256 dot-agent-deck-*` cannot cope \
         with the space in Tauri's `Agent Deck_<version>_amd64.deb`."
    );
}

#[test]
fn desktop_artifacts_are_named_outside_the_cli_pattern() {
    let all = jobs(&workflow());
    let bundle = job(&all, "desktop-bundle");

    assert!(
        bundle.contains("name: desktop-bundle-${{ matrix.asset_suffix }}"),
        "`desktop-bundle`'s upload-artifact name must stay outside the \
         `dot-agent-deck-*` pattern that `finalize` selects on. Naming the \
         ARTIFACT `dot-agent-deck-desktop-…` would re-open the collision that \
         `pattern:` closes — note the FILES inside are named \
         `dot-agent-deck-desktop-alpha-*` on purpose; it is the artifact name \
         that must differ."
    );
}

/// Greptile P1 on #768, and a good catch: `fail-fast: false` on the matrix was
/// doing less than it looked like. It stops a failing leg from CANCELLING its
/// sibling, so both legs run and both upload — but `needs:` on a matrix job
/// resolves to the AGGREGATE result, so a single failed leg still skipped
/// `desktop-publish` and threw the surviving platform's bundle away. The bundle
/// was built, uploaded, and then silently discarded.
///
/// The fix is to gate `desktop-publish` on the release object existing rather
/// than on every leg succeeding. This test exists because the original wiring
/// tests asserted the `needs:` edges and still missed it: the edge was right,
/// its aggregate semantics were not.
#[test]
fn a_failed_platform_leg_still_publishes_the_other() {
    let all = jobs(&workflow());
    let publish = job(&all, "desktop-publish");

    let condition = publish
        .lines()
        .find(|l| l.trim_start().starts_with("if:"))
        .unwrap_or_else(|| panic!("desktop-publish has no `if:` condition:\n{publish}"));

    assert!(
        condition.contains("!cancelled()"),
        "`desktop-publish` must run with `!cancelled()` rather than inheriting \
         the aggregate success of the `desktop-bundle` matrix. Without it, one \
         platform's bundler failure skips publication entirely and discards the \
         OTHER platform's good bundle — which `fail-fast: false` cannot prevent, \
         because it governs cancellation, not the aggregate result.\n\
         Found: {condition}"
    );
    assert!(
        condition.contains("needs.finalize.result == 'success'"),
        "`desktop-publish` must still require that `finalize` succeeded — there \
         is no release object to upload to otherwise, and `!cancelled()` alone \
         would let it try.\nFound: {condition}"
    );
}

/// The other half of the same fix: publishing a partial matrix is allowed, but
/// it must be loud. A release carrying one platform when it should carry two
/// is not something to discover from a user's bug report.
#[test]
fn a_partial_desktop_publish_is_not_silent() {
    let all = jobs(&workflow());
    let publish = job(&all, "desktop-publish");

    assert!(
        publish.contains("::warning::no bundle for"),
        "`desktop-publish` must warn per missing platform when it publishes a \
         partial set, and must fail outright when there is nothing to publish."
    );
    assert!(
        publish.contains("::error::every desktop-bundle leg failed"),
        "`desktop-publish` must fail with an explicit message when no leg \
         produced an artifact, rather than surfacing download-artifact's \
         `Unable to find any artifacts` and leaving the cause to be guessed."
    );
}

/// A green run with a missing artifact is the failure mode that matters here,
/// so the desktop jobs must be allowed to go red.
///
/// Scoped to JOB-level `continue-on-error`, which is the one that suppresses a
/// whole job's result. A STEP-level one is a different thing and is legitimate:
/// `desktop-publish`'s download step carries it precisely so a partial matrix
/// reaches the guard step that reports which platform is missing, instead of
/// dying on `download-artifact`'s own less useful error. An earlier version of
/// this test forbade the string anywhere in the block and so fired on that fix
/// — right instinct, wrong resolution.
#[test]
fn desktop_jobs_do_not_swallow_their_own_failures() {
    let all = jobs(&workflow());
    for name in ["desktop-bundle", "desktop-publish"] {
        let block = job(&all, name);
        let job_level = block.lines().any(|l| {
            l.starts_with("    ")
                && !l.starts_with("     ")
                && l.trim_start().starts_with("continue-on-error:")
        });
        assert!(
            !job_level,
            "`{name}` sets a JOB-level continue-on-error. The release is \
             already protected by the job graph (see \
             finalize_does_not_wait_on_the_desktop_jobs), so this only hides a \
             failure: the run stays green and a silently missing artifact \
             becomes indistinguishable from a healthy release. PRD #740 \
             Decision 4."
        );
    }
}

/// Not a wiring property, but the same class of single line that reads as
/// housekeeping: without it, one platform's bundler failure cancels the other's
/// in-flight job and a recoverable macOS build is thrown away.
#[test]
fn one_platforms_bundler_failure_does_not_cancel_the_other() {
    let all = jobs(&workflow());
    let bundle = job(&all, "desktop-bundle");
    assert!(
        bundle.contains("fail-fast: false"),
        "`desktop-bundle` must set `fail-fast: false`, or an AppImage fetch \
         timeout on Linux cancels an otherwise-good macOS dmg."
    );
}
