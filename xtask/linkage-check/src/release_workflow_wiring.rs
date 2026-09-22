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

    for forbidden in ["desktop-bundle", "desktop-publish", "attest"] {
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

/// PRD #757 takes the free half of artifact trust: every published asset gets
/// SLSA build provenance from `actions/attest-build-provenance`, signed with a
/// short-lived Sigstore certificate obtained through the job's OIDC token. It
/// is the only half that reaches Linux at all, since `dpkg -i` verifies no
/// package signature.
///
/// Two properties of the job matter and neither is visible in review:
///
/// It must stay **off the critical path**, like `desktop-bundle` before it. An
/// attestation-service blip must turn the run red and leave the CLI release
/// complete, which is only true while `finalize` does not wait on it. That half
/// is covered by [`finalize_does_not_wait_on_the_desktop_jobs`], extended here
/// to name `attest` too.
///
/// And it must **not be gated on the desktop matrix succeeding**. `needs:` on a
/// matrix job resolves to the aggregate, so `needs: desktop-publish` alone would
/// skip attestation of the CLI binaries whenever a bundler leg failed --
/// exactly the defect Greptile's P1 on #768 found in `desktop-publish` itself.
/// The gate is on the release object existing.
#[test]
fn attestation_is_off_the_critical_path_and_not_hostage_to_the_desktop_matrix() {
    let all = jobs(&workflow());
    let attest = job(&all, "attest");

    let finalize = needs(job(&all, "finalize"));
    assert!(
        !finalize.iter().any(|n| n == "attest"),
        "`finalize` lists `attest` in needs: {finalize:?}. Attestation calls an \
         external service from inside the release path; putting it ahead of the \
         job that creates the Release means a service blip delays or blocks an \
         ordinary tag. Same topology rule as the desktop jobs -- PRD #740 \
         Decision 4, PRD #757."
    );

    let gate = attest
        .lines()
        .map(code_before_comment)
        .find(|l| l.trim_start().starts_with("if:"))
        .unwrap_or_else(|| panic!("`attest` has no `if:` gate:\n{attest}"));
    assert!(
        gate.contains("needs.finalize.result == 'success'"),
        "`attest` must gate on the release object existing, got `{}`. Gating on \
         `desktop-publish` succeeding instead would discard the CLI binaries' \
         attestations every time a bundler leg failed, because `needs:` on a \
         matrix job resolves to the AGGREGATE result. That is the defect \
         Greptile's P1 on #768 found one job over.",
        gate.trim()
    );
}

/// The whole security argument for attestation is that it needs no secret: the
/// signing certificate is short-lived and comes from Sigstore via the job's
/// OIDC token, so there is no private key in this repository to leak, rotate,
/// or have revoked. PRD #757 Decision 5 is what the alternative would have
/// cost.
///
/// That argument is a property of the job's `permissions:` block, and the
/// natural future edit -- someone adds a credential here to sign something as
/// well -- is a one-line change that reads as a convenience. So the block is
/// pinned: exactly the three scopes attestation needs, and nothing that could
/// carry or reach a signing key.
#[test]
fn the_attest_job_holds_no_credential_and_needs_none() {
    let all = jobs(&workflow());
    let attest = job(&all, "attest");
    let code: String = attest
        .lines()
        .map(code_before_comment)
        .collect::<Vec<_>>()
        .join("\n");

    for required in ["id-token: write", "attestations: write"] {
        assert!(
            code.contains(required),
            "`attest` must declare `{required}`. Job-level permissions REPLACE \
             the workflow-level set, so omitting one does not fall back to a \
             default -- the OIDC token or the attestation write is simply \
             absent and the action fails at the end of a release."
        );
    }
    assert!(
        !code.contains("contents: write"),
        "`attest` must not take `contents: write`. It downloads artifacts and \
         calls the attestation API; nothing in it writes to the repository, and \
         a job-level permissions block is the one place that scope can be \
         narrowed from the workflow-level default."
    );
    assert!(
        !code.contains("secrets."),
        "`attest` references a secret. The entire argument for provenance over \
         code signing (PRD #757 Decisions 2 and 5) is that it needs no \
         credential -- the certificate is short-lived and comes from Sigstore \
         through `id-token`. A secret here means something else is going on, \
         and it should be reviewed rather than inherited."
    );
}

/// The code lines of `desktop-publish`'s "Note the unsigned alpha in the
/// release body" step, from its `name:` to the next step at the same
/// indentation.
///
/// Scoped to the step rather than the job because the two assertions below are
/// substring checks, and over a whole job block a substring can be satisfied by
/// something that is not the note at all -- a later step that happens to
/// mention `dpkg`, or a `System Settings` reference in an unrelated `echo`.
/// The guard would then pass while the note itself had been gutted, which is
/// the one outcome it exists to prevent. (Greptile's read of PR #879, and a
/// fair one.)
///
/// Comments are stripped by [`code_before_comment`] for the reason its own
/// docstring gives, and it matters in the worst direction here: the note step
/// carries prose *about* the routes it offers, so an unstripped block could be
/// satisfied by a comment explaining the rule instead of by the rule.
fn note_step(block: &str) -> String {
    let mut lines = block
        .lines()
        .skip_while(|l| !l.contains("Note the unsigned alpha"));
    let first = lines.next().unwrap_or_else(|| {
        panic!("`desktop-publish` has no \"Note the unsigned alpha\" step:\n{block}")
    });
    let indent = first.len() - first.trim_start().len();
    std::iter::once(first)
        .chain(lines.take_while(|l| {
            let trimmed = l.trim_start();
            trimmed.is_empty() || l.len() - trimmed.len() > indent || !trimmed.starts_with("- ")
        }))
        .map(code_before_comment)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The release-body note is the only installation instruction this project
/// publishes for the desktop bundles -- PRD #740 Decision 11 ships the alpha
/// unadvertised, so there is no `docs/` page to correct it. Two properties of
/// it are load-bearing enough to guard, and both were defects until PRD #757.
///
/// **A no-terminal route must exist wherever the terminal one does.** macOS
/// Sequoia removed the Control-click Gatekeeper bypass, so a note offering
/// only `xattr -dr com.apple.quarantine` leaves a user who will not run a
/// recursive command against `/Applications` with nothing at all -- and
/// teaches everyone else a habit that transfers to the next thing that asks
/// for it. Written as an implication rather than a presence check so it stays
/// correct in both directions: once the macOS bundle is signed and the `xattr`
/// line is deleted (PRD #757 Decision 10), this passes vacuously rather than
/// having to be edited in the same commit.
///
/// **The Linux asset must be addressed on its own terms.** The note used to
/// say the assets "are unsigned, so your OS will warn you", which is simply
/// false for the `.deb`: `dpkg` ships with per-package signature verification
/// disabled (`no-debsig`), so `dpkg -i` checks nothing and warns about
/// nothing. Naming `dpkg` is the cheapest observable proxy for "somebody
/// wrote a Linux sentence" -- a single sentence written for macOS cannot
/// satisfy it.
#[test]
fn the_alpha_note_does_not_leave_a_user_with_only_a_terminal_command() {
    let all = jobs(&workflow());
    let code = note_step(job(&all, "desktop-publish"));

    if code.contains("com.apple.quarantine") {
        assert!(
            code.contains("System Settings"),
            "the release note offers `xattr -dr com.apple.quarantine` as its \
             only macOS route. Control-clicking an app to bypass Gatekeeper no \
             longer works on macOS Sequoia and later, so the System Settings > \
             Privacy & Security > Open Anyway route has to be there too -- \
             otherwise a user unwilling to run a recursive command against \
             /Applications is given nothing. PRD #757 Decision 3."
        );
    }

    assert!(
        code.contains("dpkg"),
        "the release note says nothing specific about the `.deb`. It used to \
         cover it with a sentence written for macOS -- \"unsigned, so your OS \
         will warn you\" -- and that clause is false for Linux: `dpkg` ships \
         with `no-debsig`, so `dpkg -i` verifies no package signature and \
         issues no warning. PRD #757 Decisions 3 and 7."
    );
}

/// The note has to establish that a download is genuine BEFORE it tells anyone
/// how to get past Gatekeeper, because the bundles are unsigned and provenance
/// is the only thing that says where the file came from. Read top-down, the
/// note used to hand over both override routes first, so a reader had
/// bypassed the protection before reaching the check that would justify it
/// (#1153). Nothing about the text changes when the order does, which is why
/// a presence check cannot hold this.
///
/// The command also has to prove what the note says it proves. `--repo` pins
/// the repository but not the workflow, so an attestation from ANY workflow
/// in it passes; only `--signer-workflow` makes "produced by this repository's
/// release workflow" true. Measured on v0.41.0's `.dmg` and `.deb`: exit 0
/// naming `release.yml`, exit 1 naming `ci.yml`.
#[test]
fn the_alpha_note_verifies_provenance_before_it_overrides_gatekeeper() {
    let all = jobs(&workflow());
    let code = note_step(job(&all, "desktop-publish"));

    let verify = code.find("gh attestation verify").unwrap_or_else(|| {
        panic!("the release note no longer carries a `gh attestation verify` command:\n{code}")
    });
    // Up to the `printf` escape that ends the rendered line, so a flag
    // elsewhere in the same format string cannot satisfy the check.
    let command = code[verify..].split("\\n").next().unwrap_or_default();
    assert!(
        command.contains("--signer-workflow") && command.contains("/.github/workflows/release.yml"),
        "the release note's verify command does not pin the signing workflow: \
         `{command}`. `--repo` alone accepts an attestation from any workflow in \
         the repository, while the note tells the user it proves the file came \
         from the release workflow. #1153."
    );
    for route in ["System Settings", "com.apple.quarantine"] {
        if let Some(at) = code.find(route) {
            assert!(
                verify < at,
                "the release note offers the `{route}` Gatekeeper override before \
                 the `gh attestation verify` command. The bundles are unsigned, so \
                 verifying provenance is the prerequisite for overriding the \
                 warning, and a reader following the note top-down must reach it \
                 first. #1153."
            );
        }
    }
}

/// The code portion of `line` -- everything before an unquoted `#` that opens a
/// trailing shell comment. A whole-line comment reduces to its indentation.
///
/// Both predicates below run through this, and neither is safe without it. The
/// steps in `desktop-publish` now carry prose *about* the absence of a checkout
/// and about `--repo`, so a comment can satisfy either check by talking about
/// it -- and for `checks_out` that failure is silent in the worst direction: a
/// trailing `# no actions/checkout here` would make the guard skip the job.
///
/// Quote-aware, because `printf "…#…"` opens no comment, and word-aware,
/// because a `#` mid-word does not either. Deliberately no more than that: it
/// does not understand backslash escapes or here-documents, and does not need
/// to, since its only job is to keep a `#`-commented mention from being read as
/// code. Erring toward treating text as code is the safe direction, since code
/// is what gets checked.
fn code_before_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double && (i == 0 || bytes[i - 1].is_ascii_whitespace()) => {
                return &line[..i];
            }
            _ => {}
        }
    }
    line
}

/// Does this job block carry an `actions/checkout` step?
fn checks_out(block: &str) -> bool {
    block
        .lines()
        .map(code_before_comment)
        .any(|l| l.contains("uses: actions/checkout"))
}

/// Does this command name a repository through a well-formed repo flag?
///
/// Token-precise rather than a `contains("--repo")`, per Greptile's P2 on
/// PR #853, which added this. A substring test also accepts `--repository` and a bare
/// trailing `--repo` with no value -- both of which `gh` itself rejects, as an
/// unknown flag and a missing argument respectively -- so it would pass
/// commands that still cannot run, which is the one thing this guard exists to
/// stop. `-R` is accepted because it is the same flag, and rejecting it would
/// fail a correct command.
fn names_a_repository(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    words.iter().enumerate().any(|(i, word)| {
        if let Some(value) = word.strip_prefix("--repo=") {
            return !value.is_empty();
        }
        if *word == "--repo" || *word == "-R" {
            // The value must be there and must not be the next flag.
            return words.get(i + 1).is_some_and(|v| !v.starts_with('-'));
        }
        false
    })
}

/// The `gh`-invoking lines of a job block, each already reduced to its code
/// portion by [`code_before_comment`] -- so a `gh` named only inside a comment
/// is not an invocation, and the `--repo` checked below is one actually passed.
///
/// Matched on a `gh` *token* -- the two characters preceded by something that
/// cannot continue an identifier, and followed by whitespace -- and not on the
/// bare substring, which is everywhere in ordinary English (`through`, `high`,
/// `right`) and in this file's own `github.token` / `GH_TOKEN` bindings. A
/// sweep that drowns in noise gets abandoned, which buys as much as not
/// sweeping.
///
/// `/` and `.` are deliberately NOT in that set, so a path-qualified
/// `/usr/bin/gh release upload` is matched too. Erring toward over-matching is
/// the safe direction here: a false positive asks for a `--repo` that does no
/// harm, while a miss is the silent broken release this exists to prevent.
///
/// Deliberately line-oriented, so it sees `$(gh release view …)` and
/// `… | gh release edit …` as invocations. It cannot see one split across a
/// backslash continuation; nothing in `release.yml` writes one, and the
/// non-vacuity assertion below is what notices if that changes.
fn gh_invocations(block: &str) -> Vec<&str> {
    block
        .lines()
        .map(code_before_comment)
        .filter(|l| {
            l.match_indices("gh").any(|(i, _)| {
                let boundary_before = l[..i]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_alphanumeric() || "_-".contains(c)));
                let arguments_after = l[i + 2..].chars().next().is_some_and(char::is_whitespace);
                boundary_before && arguments_after
            })
        })
        .collect()
}

/// Issue #852: `desktop-publish` is the one job in this file with no
/// `actions/checkout` -- it downloads artifacts and calls the API, so it needs
/// no source tree -- and none of its three `gh` calls passed `--repo`. `gh`
/// then falls back to inspecting git for its target repository, finds none, and
/// dies with `failed to run git: fatal: not a git repository`. Every release
/// that reached the job failed inside it, so on every tag carrying these jobs
/// the desktop alpha bundles were built correctly and then never attached, and
/// the release-body note the two later calls append was never appended either.
///
/// Asserted over every `gh` call in every checkout-less job rather than over
/// the three lines that were wrong, and that generality is the point: the
/// steps run under the default `bash -e`, so the first failure aborts the job
/// and the other two calls were *latent*. A check pinned to the call named in
/// the failure log would have been satisfied by a fix that merely relocated the
/// failure to the next call. Stated as a property it also covers the next job
/// added without a checkout, which is how this one arrived.
///
/// Either remedy satisfies it, matching the two the issue offers: a `--repo` on
/// each call, or a checkout for the job. This does not pin which.
///
/// **Scoped to `release.yml`**, which is this module's file and the workflow
/// that never runs on a pull request -- so a static assertion is the only thing
/// that can catch this at all. It is *not* a claim about the other workflows,
/// and does not generalise to them as written: `ci.yml`'s `changes` job reaches
/// the API as `gh api repos/$REPO/…`, carrying its repository in the request
/// path rather than in a flag, and the `*.lock.yml` files are generated.
#[test]
fn gh_calls_in_checkoutless_jobs_name_their_repository() {
    let all = jobs(&workflow());

    for (name, block) in &all {
        if checks_out(block) {
            continue;
        }
        for call in gh_invocations(block) {
            assert!(
                names_a_repository(call),
                "`{name}` has no `actions/checkout` step, so its workspace \
                 holds no git repository for `gh` to infer a target from -- and \
                 this call names none either (shown as parsed, with any \
                 trailing comment removed):\n\n    {}\n\n\
                 It will fail with `failed to run git: fatal: not a git \
                 repository`. That is issue #852: from PRD #740 onward every \
                 release built the desktop bundles and then failed to attach \
                 them, because all three `gh` calls in `desktop-publish` were \
                 missing this flag. Fix it either way -- add `--repo \"$REPO\"` \
                 with `REPO: ${{{{ github.repository }}}}` in the step's \
                 `env:`, or give the job a SHA-pinned `actions/checkout` -- but \
                 note the flag must carry a VALUE and be real code: \
                 `--repository`, a bare `--repo`, and a `--repo` inside a \
                 comment are all rejected here because `gh` rejects them too -- \
                 fix it here, because `release.yml` fires only on a tag and \
                 nothing in CI will tell you. By the time this is observable a \
                 release has already gone out without its assets.",
                call.trim()
            );
        }
    }

    // Non-vacuity. A guard that passes because it matched nothing is not a
    // guard, and this one is one `gh`-token predicate away from matching
    // nothing. `desktop-publish` is the only job in the file that reaches the
    // API through the CLI, and it does so on three separate lines; counted
    // regardless of whether the job checks out, so this stays live under either
    // remedy above.
    let publish_lines = gh_invocations(job(&all, "desktop-publish")).len();
    assert!(
        publish_lines >= 3,
        "expected `desktop-publish` to still hold the three `gh` invocation \
         LINES that issue #852 was about, matched {publish_lines}. Lines, not \
         calls -- two invocations sharing a line would count once -- and a \
         lower bound rather than a pin on the step layout: if a call was \
         deliberately removed, lower it in the same commit. If all three are \
         still there, the `gh` token match above has stopped seeing them -- \
         perhaps a call is now split across a line continuation -- and the loop \
         it feeds has become a no-op that passes on nothing."
    );
}

// ---------------------------------------------------------------------------
// Issue #1219: the release body is clamped to GitHub's 125,000-character cap.
// ---------------------------------------------------------------------------
//
// v0.41.1 shipped 131,299 characters of notes. GitHub silently truncated the
// body to exactly 125,000, ending mid-sentence, and then rejected
// `desktop-publish`'s append with `HTTP 422: body is too long` -- so the .dmg
// and .deb went out with no Gatekeeper instructions and no provenance command.
// `prepare` now clamps the body itself, on an entry boundary, reserving room
// for the note that job appends afterwards.
//
// Nothing else covers this. `release.yml` fires only on a tag, so the clamp
// runs in no PR and its first execution is a real release -- the same argument
// this module's header makes for the wiring assertions, and the reason
// Greptile asked for coverage on PR #1221. These tests close that by lifting
// the heredoc out of the workflow and running it, which is exactly what
// `junit_strip` does with `scripts/junit-strip-output.py` (CLAUDE.md rule 5
// names it as an accepted exception, on the same ground: the safety property
// is a runtime assertion and nothing else).

/// The Python source inside `prepare`'s `<<'CLAMP'` heredoc, dedented so it
/// can be fed to an interpreter.
///
/// Extracted rather than duplicated. A copy of the script in this file would
/// pass forever while the workflow's own copy rotted, which is the failure
/// mode a linkage check exists to prevent.
fn clamp_script() -> String {
    let text = workflow();
    let mut lines = text.lines().skip_while(|l| !l.contains("<<'CLAMP'"));
    assert!(
        lines.next().is_some(),
        "`prepare` no longer contains a `<<'CLAMP'` heredoc. If the clamp moved \
         or was replaced, move these tests with it -- do not delete them: the \
         125,000-character cap is still there and still silent."
    );
    let body: Vec<&str> = lines.take_while(|l| l.trim() != "CLAMP").collect();
    assert!(!body.is_empty(), "the CLAMP heredoc is empty");
    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .expect("the heredoc has at least one non-blank line");
    body.iter()
        .map(|l| if l.len() >= indent { &l[indent..] } else { "" })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run the extracted clamp over `body`, returning what it left on disk.
///
/// `None` means `python3` is absent and the caller should print `SKIP:` and
/// return, matching `junit_strip`.
fn run_clamp(body: &str) -> Option<String> {
    use std::process::Command;
    Command::new("python3").arg("--version").output().ok()?;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("release-body.md");
    fs::write(&path, body).expect("write body");
    let script = dir.path().join("clamp.py");
    fs::write(&script, clamp_script()).expect("write script");
    let out = Command::new("python3")
        .arg(&script)
        .arg(&path)
        .output()
        .expect("run the clamp");
    assert!(
        out.status.success(),
        "the clamp exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    // Normalize CRLF, because on Windows we would otherwise be measuring the
    // TEST HARNESS rather than the clamp. Python's text mode translates each
    // `\n` it writes into `\r\n` there, so the readback carries one extra
    // character per line -- 562 of them on the dense fixture, which is what
    // turned `build-windows` red on the first push of this PR with
    // `clamped body is 122562 chars, over the 122000 budget`.
    //
    // The clamp itself was correct and the number it enforces is the right
    // one: `prepare` runs on `ubuntu-latest`, Python writes LF there with no
    // translation, and the artifact travels to `finalize` byte for byte -- so
    // the length GitHub applies its 125,000-character cap to is the LF length,
    // which is exactly what the clamp counted. Measuring the CRLF form would
    // assert against a body that is never published.
    //
    // Normalizing rather than gating the test on `cfg(unix)`: every other
    // assertion here (entries kept whole, the giant entry dropped, the pointer
    // appended) is platform-independent and worth running everywhere. This
    // crate already treats CRLF as a known Windows hazard rather than a reason
    // to skip -- see `skill_frontmatter`'s `a_crlf_checkout_parses_the_same_as_lf`.
    Some(
        fs::read_to_string(&path)
            .expect("read back")
            .replace("\r\n", "\n"),
    )
}

/// The two constants the clamp is parameterised by, read from its own source
/// so a test can never assert against a number the workflow has since changed.
fn clamp_limits() -> (usize, usize) {
    let script = clamp_script();
    let line = script
        .lines()
        .find(|l| l.starts_with("LIMIT, RESERVE ="))
        .expect("the clamp declares `LIMIT, RESERVE = ...`");
    let mut nums = line
        .split('=')
        .nth(1)
        .expect("an assignment")
        .split(',')
        .map(|n| n.trim().parse::<usize>().expect("an integer"));
    (nums.next().unwrap(), nums.next().unwrap())
}

/// A body of `entries` changelog entries, each spanning several physical
/// lines, in the shape `assemble-changelog.sh` actually produces.
///
/// The multi-line entry is the whole point: a clamp that cut on `\n` would
/// land *inside* one of these and publish half of it, which is the defect
/// [`an_oversized_body_is_cut_between_entries_never_inside_one`] exists to
/// catch. CLAUDE.md rule 10 means real entries are long unwrapped single
/// lines with continuations, so this mirrors that rather than a wrapped
/// paragraph.
fn synthetic_body(entries: usize) -> String {
    let mut s = String::from("## What's changed\n\n");
    for i in 0..entries {
        s.push_str(&format!(
            "- **entry-{i:04}-open** {}\n  continuation one for entry {i:04}\n  \
             continuation two for entry {i:04} entry-{i:04}-close\n",
            "padding ".repeat(60)
        ));
    }
    s
}

/// A body whose entry boundaries are SPARSE: three ordinary entries, then one
/// enormous entry whose continuation lines run past the budget on their own.
///
/// This is the shape that exposes the bug, and a dense fixture does not --
/// found by mutation-testing this very file. With entries packed every few
/// hundred characters, `rfind("\n- **")` always lands close to the budget, so
/// the sparse branch is never taken and a test built on that fixture passes
/// against the broken clamp as happily as against the fixed one. Here the last
/// boundary sits a few hundred characters into a ~130,000-character body, which
/// is precisely when the old code abandoned it for `rfind("\n")` and cut
/// through the middle of the giant entry.
///
/// Not a contrived shape, either: one release note carrying a long rationale --
/// which CLAUDE.md rule 10 writes as a single unwrapped line -- produces it.
fn synthetic_sparse_body(limit: usize) -> String {
    let mut s = String::from("## What's changed\n\n");
    for i in 0..3 {
        s.push_str(&format!(
            "- **entry-{i:04}-open** short one\n  continuation entry-{i:04}-close\n"
        ));
    }
    s.push_str("- **entry-9999-open** the entry that dwarfs the budget\n");
    while s.chars().count() < limit + 5_000 {
        s.push_str("  a continuation line inside the giant entry, nowhere near its end\n");
    }
    s.push_str("  entry-9999-close\n");
    s
}

/// Every `- **` entry the clamp keeps must be kept WHOLE.
///
/// This is the finding Greptile raised on PR #1221, and it was real: the first
/// implementation dropped to `rfind("\n")` whenever the last entry boundary
/// fell in the first half of the budget, on the theory that an early boundary
/// wasted room. An entry spans several lines, so that cut landed inside one --
/// reintroducing the mid-sentence truncation the clamp was written to prevent,
/// committed by us rather than by GitHub.
///
/// The assertion is over the *pairing*: an entry contributes an `-open` marker
/// and, three lines later, a `-close` one. Counting them separately is what
/// makes a split detectable at all -- a length check passes just as happily on
/// a body cut through the middle of an entry.
#[test]
fn an_oversized_body_is_cut_between_entries_never_inside_one() {
    let (limit, reserve) = clamp_limits();
    // Sized so the cut lands mid-entry unless the clamp seeks a boundary.
    let body = synthetic_body(400);
    assert!(
        body.chars().count() > limit,
        "the fixture must exceed the cap to exercise the clamp"
    );
    let Some(out) = run_clamp(&body) else {
        eprintln!("SKIP: the release-body clamp test needs `python3` on PATH");
        return;
    };

    assert!(
        out.chars().count() <= limit - reserve,
        "clamped body is {} chars, over the {} budget",
        out.chars().count(),
        limit - reserve
    );
    let opened = out.matches("-open**").count();
    let closed = out.matches("-close").count();
    assert_eq!(
        opened, closed,
        "the clamp cut INSIDE an entry: {opened} entries start in the output \
         but {closed} finish. The cut must land on a `\\n- **` boundary, so \
         that every entry published is published whole."
    );
    assert!(opened > 0, "the clamp kept no entries at all");
    assert!(
        out.contains("CHANGELOG.md"),
        "a truncated body must point at the full text"
    );
}

/// A body that already fits is left byte-identical.
///
/// The clamp runs on every release, almost all of which are far under the cap,
/// so the common path is the one where it must do nothing -- no pointer
/// appended, no trailing whitespace stripped, no entry dropped.
#[test]
fn a_body_under_the_cap_is_left_exactly_as_it_was() {
    let body = synthetic_body(3);
    let Some(out) = run_clamp(&body) else {
        eprintln!("SKIP: the release-body clamp test needs `python3` on PATH");
        return;
    };
    assert_eq!(
        out, body,
        "the clamp modified a body that was already under the cap"
    );
}

/// `RESERVE` must cover the note `desktop-publish` appends after the release
/// exists.
///
/// This is the half of v0.41.1 that actually cost users something: the body
/// was already at the cap, so the append was rejected with `HTTP 422` and the
/// desktop bundles shipped with no install instructions and no
/// `gh attestation verify` command. Clamping to the cap alone would not have
/// prevented that -- only clamping to the cap *minus room for this note* does.
///
/// The two live in different jobs with nothing between them, which is exactly
/// the shape this module exists to guard: the note is prose and will be edited
/// again, and there is no reason its author would think to look at a Python
/// constant in `prepare`.
///
/// Measured over the raw `printf` arguments, which OVERSTATES the rendered
/// length (`\n` is two characters here and one on the wire, `\`` is two and
/// one), so the check errs toward demanding more headroom than is strictly
/// needed. It is a floor, not an equality.
#[test]
fn the_reserve_covers_the_note_desktop_publish_appends() {
    let all = jobs(&workflow());
    let note = note_step(job(&all, "desktop-publish"));
    let printed: usize = note
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("printf"))
        .map(str::len)
        .sum();
    assert!(
        printed > 0,
        "found no `printf` lines in the alpha-note step"
    );

    let (_, reserve) = clamp_limits();
    assert!(
        reserve >= printed,
        "`desktop-publish`'s alpha note is ~{printed} characters but `prepare` \
         reserves only {reserve} for it. A clamped release body would be \
         accepted and the note that follows it rejected with `HTTP 422: body \
         is too long` -- the v0.41.1 failure exactly. Raise RESERVE in the \
         CLAMP heredoc in `.github/workflows/release.yml`."
    );
}

/// The sparse case, which is the one Greptile actually reported.
///
/// When no entry boundary falls in the latter part of the budget, the clamp
/// must still cut at the boundary it *did* find, however early -- keeping less
/// rather than publishing a fragment. The old code preferred a whole physical
/// line here, which is not a unit of anything: entries span several lines, so
/// that cut landed inside one.
///
/// [`an_oversized_body_is_cut_between_entries_never_inside_one`] does NOT
/// cover this. Its fixture is dense, so the branch below is never reached and
/// the broken clamp passes it. Keep both.
#[test]
fn a_sparse_body_is_cut_at_the_early_boundary_not_at_a_line() {
    let (limit, _) = clamp_limits();
    let body = synthetic_sparse_body(limit);
    let Some(out) = run_clamp(&body) else {
        eprintln!("SKIP: the release-body clamp test needs `python3` on PATH");
        return;
    };

    let opened = out.matches("-open**").count();
    let closed = out.matches("-close").count();
    assert_eq!(
        opened, closed,
        "the clamp cut INSIDE an entry when boundaries were sparse: {opened} \
         entries start in the output but {closed} finish. Cutting at the last \
         `\\n- **` however early it falls keeps less text; cutting at an \
         arbitrary `\\n` publishes half an entry. Keep less."
    );
    assert!(
        !out.contains("entry-9999-open"),
        "the giant entry was opened but cannot be finished -- it must be \
         dropped whole, not truncated"
    );
    assert!(opened > 0, "the clamp kept no entries at all");
}

/// A body with no newline at all still gets clamped, rather than losing one
/// character and being published over the cap.
///
/// `rfind` returns -1 on no match, and `src[:-1]` is not a clamp: it drops a
/// single character and reports success. Measured on the unguarded version, a
/// 131,299-character body came back **131,311** -- longer than it started,
/// because the appended pointer outweighs the removed character -- whereupon
/// GitHub truncates it silently and the clamp has caused the exact failure it
/// exists to prevent, having printed a reassuring line on the way.
///
/// `assemble-changelog.sh` does not produce such a body. That is not the
/// point: this whole step exists because the cap was reached by a release
/// nobody predicted, and a guard whose own failure mode is silent is worse
/// than no guard.
#[test]
fn a_body_with_no_line_breaks_is_still_clamped() {
    let (limit, reserve) = clamp_limits();
    let body = "x".repeat(limit + 6_000);
    let Some(out) = run_clamp(&body) else {
        eprintln!("SKIP: the release-body clamp test needs `python3` on PATH");
        return;
    };
    assert!(
        out.chars().count() <= limit - reserve,
        "a body with no newline came back {} chars, over the {} budget -- the \
         `rfind` miss fell through to a negative index instead of clamping",
        out.chars().count(),
        limit - reserve
    );
    assert!(
        out.chars().count() < body.chars().count(),
        "the clamp returned a body no shorter than it received"
    );
}

/// The step blocks of a job, split on the `      - ` step markers.
///
/// Comments written *above* a step belong to the preceding step's block, since
/// the `- ` line is what opens a new one. That is harmless for the callers
/// below, which read only `uses:`/`name:`/`path:` code lines -- but it is worth
/// knowing before writing a check here that greps a step for prose.
fn steps(block: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in block.lines() {
        if line.starts_with("      - ") {
            if let Some(body) = current.take() {
                out.push(body.join("\n"));
            }
            current = Some(vec![line.to_string()]);
            continue;
        }
        if let Some(body) = current.as_mut() {
            body.push(line.to_string());
        }
    }
    if let Some(body) = current.take() {
        out.push(body.join("\n"));
    }
    out
}

/// The code lines of `step`, with trailing shell comments removed.
fn step_code(step: &str) -> String {
    step.lines()
        .map(code_before_comment)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `name:` of the `actions/upload-artifact` step in `block` whose `path:`
/// is exactly `path`, together with whether it declares
/// `if-no-files-found: error`.
///
/// Keyed on the PATH rather than on the artifact name, so the name itself stays
/// an output of the check rather than an input to it. That is the whole point:
/// the assertions below then compare the producer's chosen name against the
/// consumer's, and a rename done in one place and not the other fails instead
/// of passing twice over a hardcoded literal.
fn uploaded_as(block: &str, path: &str) -> Option<(String, bool)> {
    steps(block).into_iter().find_map(|step| {
        let code = step_code(&step);
        if !code.contains("actions/upload-artifact@") {
            return None;
        }
        if !code.lines().any(|l| l.trim() == format!("path: {path}")) {
            return None;
        }
        let name = code
            .lines()
            .find_map(|l| l.trim().strip_prefix("name: ").map(str::to_string))?;
        let strict = code.lines().any(|l| l.trim() == "if-no-files-found: error");
        Some((name, strict))
    })
}

/// Whether `block` has an `actions/download-artifact` step pulling exactly the
/// artifact `name` (as `name:`, not as a `pattern:`).
fn downloads_artifact(block: &str, name: &str) -> bool {
    steps(block).iter().any(|step| {
        let code = step_code(step);
        code.contains("actions/download-artifact@")
            && code.lines().any(|l| l.trim() == format!("name: {name}"))
    })
}

/// Issue #1152: a full release publishes eight assets and the subject
/// collection used to match six of them. The two it missed were the checksum
/// manifests -- which is the worst pair to miss, because a manifest is a list
/// of hashes a user leans on to validate everything else, so an unattested one
/// is precisely the file worth swapping.
///
/// **Neither manifest is reachable from `attest` by default, and that is what
/// this guards.** `checksums.txt` is written inside `finalize` and
/// `checksums-desktop-alpha.txt` inside `desktop-publish`; `attest` has no
/// checkout and neither generates them, so each has to travel as an artifact of
/// its own. The obvious-looking alternative -- re-running `shasum` in `attest`
/// -- is the one outcome that must not happen: it would mint provenance over
/// bytes that are very probably identical to the published ones and provably
/// nothing, and an attestation over a file that differs from the published one
/// is worse than no attestation at all. So the absence of `shasum` from this
/// job is asserted too.
///
/// Nothing else can catch a regression here. `release.yml` fires only on a tag,
/// never on a pull request, and a dropped subject is invisible in the run: the
/// action succeeds over whatever list it is handed. The symptom is a `404` from
/// `gh attestation verify`, weeks later, on a release that has already shipped.
#[test]
fn both_checksum_manifests_are_attested_subjects() {
    let all = jobs(&workflow());
    let attest = job(&all, "attest");
    // Comments stripped for both substring checks below, so a comment ABOUT the
    // wiring cannot stand in for the wiring -- the same reason `note_step`'s
    // callers strip them. The attest job is full of prose naming these files.
    let attest_code = step_code(attest);

    let cases = [
        ("finalize", "dist/checksums.txt"),
        (
            "desktop-publish",
            "dist-desktop/checksums-desktop-alpha.txt",
        ),
    ];

    for (producer, path) in cases {
        let (artifact, strict) = uploaded_as(job(&all, producer), path).unwrap_or_else(|| {
            panic!(
                "`{producer}` no longer uploads `{path}` as an artifact. It is written in that \
                 job and published from there, and `attest` has no checkout -- so without this \
                 hand-off the manifest is unreachable from the job that attests, and the only \
                 way to attest it would be to regenerate it, which attests bytes the release \
                 does not carry. #1152."
            )
        });

        // The two `pattern:` downloads that already exist would sweep a
        // carelessly named manifest artifact into a directory it must not reach:
        // `dot-agent-deck-*` is what `finalize` selects on and feeds to
        // `task checksums`' own `shasum -a 256 dot-agent-deck-*`, and
        // `desktop-bundle-*` is what `desktop-publish` re-uploads wholesale with
        // `gh release upload dist-desktop/*`.
        for reserved in ["dot-agent-deck-", "desktop-bundle-"] {
            assert!(
                !artifact.starts_with(reserved),
                "`{producer}` uploads `{path}` as artifact `{artifact}`, which matches the \
                 `{reserved}*` download pattern already in this workflow. A re-run of that job \
                 alone would then pull a previous attempt's manifest back into its own working \
                 directory. Same rule as `desktop-bundle`'s artifact name."
            );
        }

        assert!(
            strict,
            "`{producer}`'s upload of `{path}` does not declare `if-no-files-found: error`. \
             Without it a missing manifest produces a warning and no artifact, and for the \
             desktop half -- whose download in `attest` is deliberately tolerated -- that is a \
             silently unattested asset rather than a failed release."
        );

        assert!(
            downloads_artifact(attest, &artifact),
            "`attest` does not download the `{artifact}` artifact that `{producer}` uploads. The \
             producer and the consumer have to name the same artifact; renaming one alone drops \
             `{path}` from the subject list without failing anything."
        );

        assert!(
            attest_code.contains(path),
            "`attest` never names `{path}`, so it is not in the subject list handed to \
             `actions/attest-build-provenance`. Downloading the artifact is half the wiring; the \
             collect step has to add the file to `subjects` as well. #1152."
        );
    }

    assert!(
        !attest_code.contains("shasum"),
        "`attest` runs `shasum`. The manifests must travel from the jobs that published them, \
         not be regenerated here: an attestation over a file that differs from the published one \
         is worse than none, and a second `shasum` run in a different job is a claim about bytes \
         nobody checked. #1152."
    );
}
