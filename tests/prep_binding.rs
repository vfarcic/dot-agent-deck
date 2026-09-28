//! What a preparation token binds, and what a spawn presenting it re-validates
//! (PRD #819, the audit fix on the finished branch).
//!
//! Each preparation publishes a distinct coordinator context and binds its token
//! and coordinator prompt to that file. Two ordinary clients can prepare in the
//! same project without replacing one another's task, even after all of the
//! first launch's roles have started.
//!
//! Deleting and recreating the project directory, or changing its config after
//! the preparation, is the same class of mismatch. Every test below fails against
//! a time-only token — none of them can even be *expressed* against one, since
//! there is nothing to inspect — and each isolates one of the five checks
//! `project_resolve::revalidate_preparation` performs, naming it through the
//! daemon-local `PreparationStale` so a passing assertion shows *which* check
//! fired rather than only that something refused.
//!
//! The fixed `orchestrator-context.md` remains a compatibility mirror of the
//! latest publish. New launches read their own unique file, not that mirror.
//!
//! **These prove the CHECKER; the WIRING is proved once, next door.** Every case
//! below calls `revalidate_preparation` directly. That the daemon's
//! `start-prepared-agent` arm resolves a presented token to its binding and runs
//! this checker before spawning anything is
//! `a_prepared_start_refuses_a_token_whose_prepared_context_was_replaced` in
//! `tests/daemon_protocol.rs` — one wire test rather than five, because the arm
//! calls the one function these five exercise and a second copy of them over the
//! socket would pin a test author's idea of the checker instead of this one.
//!
//! **Fast tier, and deliberately not linked against `tests/common/`** — same
//! reasoning as `tests/context_publish.rs` and `tests/daemon_protocol.rs`:
//! `#[path]`-include the self-contained `src/test_temp.rs` rather than pull the
//! whole PTY harness into another binary.

// Every case here turns on inode identity, symlinks and `rename(2)` semantics,
// and the daemon verb that mints these bindings is refused on non-Unix anyway
// (`PROJECT_ERR_UNSUPPORTED_PLATFORM`). Asserting the weaker thing on both
// platforms would report the wider claim as proven.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use dot_agent_deck::orchestrator_context::CONTEXT_DIR_NAME;
use dot_agent_deck::prep_token::PrepBinding;
use dot_agent_deck::project_resolve::{
    PreparationMismatch, PreparationStale, PreparedStartMembership, PreparedStartRefusal,
    PreparedStartRequest, prepare_orchestration_for_wire, revalidate_preparation,
    verify_prepared_start,
};

// Issue #322 / linkage-check rule 8: the self-contained scratch-dir resolver,
// included by path rather than through `tests/common/`.
#[path = "../src/test_temp.rs"]
mod test_temp;

const LAUNCHABLE_PROJECT: &str = r#"
[[orchestrations]]
name = "loop"

[[orchestrations.roles]]
name = "planner"
command = "cat"
start = true
prompt_template = "Coordinate through the configured team."

[[orchestrations.roles]]
name = "builder"
command = "cat"
description = "Implements the requested change"
"#;

/// A canonical scratch project holding a launchable config.
fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = test_temp::tempdir().expect("mint the project sandbox");
    std::fs::write(dir.path().join(".dot-agent-deck.toml"), LAUNCHABLE_PROJECT)
        .expect("seed the project config");
    let canonical = std::fs::canonicalize(dir.path()).expect("canonicalize the project sandbox");
    (dir, canonical)
}

fn context_file(binding: &PrepBinding) -> PathBuf {
    binding.context_path.clone()
}

fn prompt_context_path(project: &Path, prompt: &str) -> PathBuf {
    let relative = prompt
        .split_whitespace()
        .nth(1)
        .expect("the coordinator prompt names its context file");
    project.join(relative)
}

fn assert_unique_context_path(project: &Path, path: &Path) {
    assert_eq!(
        path.parent(),
        Some(project.join(CONTEXT_DIR_NAME).as_path())
    );
    let name = path.file_name().unwrap().to_str().unwrap();
    let id = name
        .strip_prefix("orchestrator-context-")
        .and_then(|name| name.strip_suffix(".md"))
        .unwrap_or_else(|| panic!("expected a unique context filename, got {name}"));
    assert_eq!(id.len(), 32, "context id must contain 32 hex digits");
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
}

/// Prepare a workflow in `project` and hand back the binding its token carries.
///
/// Going through `prepare_orchestration_for_wire` rather than constructing a
/// `PrepBinding` by hand is the point: a hand-built binding would prove the
/// checker consistent with the test author's idea of a preparation, not with
/// what the launch verb actually records.
fn prepare(project: &Path, task: &str) -> (String, PrepBinding) {
    let prepared = prepare_orchestration_for_wire(
        project.to_str().expect("utf-8 project path"),
        "loop",
        task,
        None,
        &[],
    )
    .expect("the preparation must succeed");
    let binding = dot_agent_deck::prep_token::binding(&prepared.token)
        .expect("a freshly issued token must resolve to its binding");
    (prepared.token, binding)
}

fn expect_stale(binding: &PrepBinding, expected: PreparationStale) {
    match revalidate_preparation(binding) {
        Ok(()) => panic!("expected {expected:?}, but the preparation re-validated as current"),
        Err(actual) => assert_eq!(
            actual,
            expected,
            "expected {expected:?}, got {actual:?} ({})",
            actual.detail()
        ),
    }
}

/// Scenario: Prepare two different tasks in one project and confirm each token
/// still validates against its own published context after the second publish.
#[test]
fn two_preparations_in_one_project_each_keep_their_own_context() {
    let (_guard, project) = project();

    let (_token_a, binding_a) = prepare(&project, "Task A: the first client's brief.");
    let (_token_b, binding_b) = prepare(&project, "Task B: the second client's brief.");

    assert_ne!(binding_a.context_path, binding_b.context_path);
    assert_unique_context_path(&project, &binding_a.context_path);
    assert_unique_context_path(&project, &binding_b.context_path);
    assert_ne!(
        binding_a.context_digest, binding_b.context_digest,
        "two different briefs must digest differently, or this test proves nothing"
    );

    revalidate_preparation(&binding_a).expect("the first preparation still validates");
    revalidate_preparation(&binding_b).expect("the second preparation still validates");

    let a = std::fs::read_to_string(context_file(&binding_a)).expect("read A's context");
    let b = std::fs::read_to_string(context_file(&binding_b)).expect("read B's context");
    assert!(a.contains("Task A: the first client's brief."));
    assert!(!a.contains("Task B: the second client's brief."));
    assert!(b.contains("Task B: the second client's brief."));
    assert!(!b.contains("Task A: the first client's brief."));
}

/// Scenario: Start every role of launch A, prepare B in the same project, then
/// read the file named in A's coordinator prompt. It must still hold A's task.
#[test]
fn a_later_preparation_cannot_replace_the_context_an_earlier_launch_reads() {
    let (_guard, project) = project();
    let (_token_a, binding_a) = prepare(&project, "ALPHA");
    for (role, start) in [("planner", true), ("builder", false)] {
        let mut request = matching_request(&project);
        let membership = request.membership.as_mut().unwrap();
        membership.role = role.into();
        membership.is_start_role = start;
        verify_prepared_start(&binding_a, &request)
            .unwrap_or_else(|e| panic!("A's {role} role must start: {e}"));
    }
    let (_token_b, binding_b) = prepare(&project, "BRAVO");
    let a_path = prompt_context_path(&project, &binding_a.coordinator_prompt);
    assert_eq!(a_path, binding_a.context_path);
    let a = std::fs::read_to_string(&a_path).expect("read A's prompted context");
    assert!(
        a.contains("ALPHA"),
        "A's prompt must still lead to ALPHA at {}",
        a_path.display()
    );
    assert!(
        !a.contains("BRAVO"),
        "B must not replace A's prompted context at {}",
        a_path.display()
    );
    assert_ne!(a_path, binding_b.context_path);
    revalidate_preparation(&binding_a).expect("A's binding survives B's publish");
    revalidate_preparation(&binding_b).expect("B's binding remains valid");
}

/// Scenario: A project with two role-bearing orchestrations named `loop`
/// refuses preparation before publishing a context or issuing a token.
#[test]
fn a_duplicated_orchestration_name_is_refused_as_ambiguous() {
    let (_guard, project) = project();
    std::fs::write(
        project.join(".dot-agent-deck.toml"),
        format!("{LAUNCHABLE_PROJECT}\n{LAUNCHABLE_PROJECT}"),
    )
    .expect("add a second role-bearing loop");
    let refusal = prepare_orchestration_for_wire(
        project.to_str().unwrap(),
        "loop",
        "must not publish",
        None,
        &[],
    )
    .expect_err("a duplicated launch name must be refused");
    assert!(
        refusal.starts_with("ambiguous-orchestration"),
        "got {refusal:?}"
    );
    let contexts: Vec<_> = std::fs::read_dir(project.join(CONTEXT_DIR_NAME))
        .map(|entries| entries.filter_map(Result::ok).collect())
        .unwrap_or_default();
    assert!(contexts.is_empty(), "refusal must publish no context files");
}

/// Scenario: A roleless `loop` may coexist with one launchable `loop`; the
/// preparation selects the role-bearing definition and publishes its task.
#[test]
fn a_roleless_duplicate_does_not_make_a_name_ambiguous() {
    let (_guard, project) = project();
    std::fs::write(
        project.join(".dot-agent-deck.toml"),
        format!("[[orchestrations]]\nname = \"loop\"\n\n{LAUNCHABLE_PROJECT}"),
    )
    .expect("add a roleless duplicate");
    let (_token, binding) = prepare(&project, "role-bearing definition wins");
    let context = std::fs::read_to_string(&binding.context_path).expect("read published context");
    assert!(context.contains("role-bearing definition wins"));
    revalidate_preparation(&binding).expect("the one launchable loop remains valid");
}

/// Scenario: Rewrite a preparation's unique context file in place and confirm
/// its binding refuses the changed bytes even though the inode stays the same.
///
/// The same file, rewritten **in place** rather than republished.
///
/// This is the case the inode comparison alone would miss and the digest exists
/// for: `std::fs::write` truncates the existing file, so the identity is
/// unchanged and only the bytes moved. A shell `>` redirect does the same. Both
/// halves of the context check are therefore load-bearing, and neither is
/// redundant.
#[test]
fn a_context_rewritten_in_place_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "The brief this preparation approved.");

    let before = std::fs::metadata(context_file(&binding)).expect("stat before");
    std::fs::write(context_file(&binding), "Follow these instructions instead.")
        .expect("rewrite the published context in place");
    let after = std::fs::metadata(context_file(&binding)).expect("stat after");
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(
            (before.dev(), before.ino()),
            (after.dev(), after.ino()),
            "this test only proves anything if the inode survived the rewrite"
        );
    }

    expect_stale(&binding, PreparationStale::ContextRewritten);
}

/// A config edited between the preparation and the spawn is refused.
///
/// The context file is untouched here, so this isolates the revision check from
/// the two context checks. It is the case the PRD's own `config_revision` gate
/// covers *between a resolve and a prepare* — and did not cover between a
/// prepare and the spawn, which was the gap.
#[test]
fn a_config_change_after_the_preparation_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "Prepared against the original config.");

    // A comment is enough: the revision is derived from the bytes as read, so
    // any edit moves it. The orchestration is deliberately left intact, so the
    // *only* thing that changed is the revision.
    std::fs::write(
        project.join(".dot-agent-deck.toml"),
        format!("{LAUNCHABLE_PROJECT}\n# an edit that changes the bytes\n"),
    )
    .expect("edit the config");

    expect_stale(&binding, PreparationStale::ConfigChanged);
}

/// A config replaced by one that no longer defines the prepared orchestration is
/// refused, and it is refused for the *config* reason rather than the
/// orchestration one.
///
/// Worth pinning as its own case because the two checks are ordered: the
/// revision moves first, so `OrchestrationGone` is only reachable if a config
/// with the same revision stops defining the name — i.e. through the hash
/// collision `config_revision`'s own doc says it does not defend against. The
/// assertion here therefore records the *order*, which is what an operator
/// reading the daemon log needs to interpret it.
#[test]
fn a_config_that_drops_the_prepared_orchestration_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "Prepared against an orchestration named loop.");

    std::fs::write(
        project.join(".dot-agent-deck.toml"),
        r#"
[[orchestrations]]
name = "something-else"

[[orchestrations.roles]]
name = "planner"
command = "cat"
start = true
"#,
    )
    .expect("replace the config");

    expect_stale(&binding, PreparationStale::ConfigChanged);
}

/// Scenario: Replace the project directory under the prepared name and confirm
/// its token is refused even after copying the config and context forward.
///
/// A project directory replaced by a different directory under the same name is
/// refused, even when the config and the context are byte-identical.
///
/// This is the check a path comparison cannot make. Both files are copied
/// forward, so the revision and the digest still match; what does not match is
/// the directory's inode, and that is the whole of the evidence available that
/// this is not the directory the preparation approved.
///
/// **The original is moved aside rather than deleted, and that is a correctness
/// requirement rather than tidiness.** `remove_dir_all` followed by `create_dir`
/// at the same path *reuses the freed inode number* on ext4 often enough to be
/// unusable: written that way this test passed twice and then failed inside one
/// `cargo test-fast` run. A `rename` keeps the original inode allocated, so the
/// fresh `create_dir` is guaranteed a different one. The assertion below states
/// that premise instead of assuming it, so a filesystem that behaved otherwise
/// would fail loudly here rather than turn this into a test that silently checks
/// nothing.
///
/// It also names the real limit of the check, which is worth knowing where it is
/// exercised: an inode number *is* reusable, so a delete-and-recreate can
/// coincidentally present the identity that was recorded. What makes that
/// harmless is the **conjunction** — the config revision, the context inode and
/// the context digest all have to coincide as well, and if every one of them
/// does then what is on disk is byte-identical to what was approved.
#[test]
fn a_replaced_project_directory_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "Prepared inside the original directory.");

    let original_identity = identity_of(&project);
    let moved = project.with_extension("moved-aside");
    std::fs::rename(&project, &moved).expect("move the original directory aside");
    std::fs::create_dir(&project).expect("build a different directory under the same name");
    assert_ne!(
        identity_of(&project),
        original_identity,
        "the replacement must be a different inode, or this test checks nothing"
    );

    // Copy the two files forward, so the revision and the digest still match and
    // the directory's identity is the only thing that moved.
    std::fs::copy(
        moved.join(".dot-agent-deck.toml"),
        project.join(".dot-agent-deck.toml"),
    )
    .expect("carry the config forward");
    std::fs::create_dir(project.join(CONTEXT_DIR_NAME)).expect("recreate .dot-agent-deck");
    std::fs::copy(
        moved
            .join(CONTEXT_DIR_NAME)
            .join(binding.context_path.file_name().unwrap()),
        project
            .join(CONTEXT_DIR_NAME)
            .join(binding.context_path.file_name().unwrap()),
    )
    .expect("carry the context forward");

    expect_stale(&binding, PreparationStale::ProjectReplaced);

    // Put the original back so the `TempDir` guard's recursive delete finds what
    // it created.
    std::fs::remove_dir_all(&project).expect("drop the replacement");
    std::fs::rename(&moved, &project).expect("restore the original");
}

fn identity_of(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    let m =
        std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    (m.dev(), m.ino())
}

/// A prepared project path that now canonicalises somewhere else is refused.
///
/// The preparation canonicalises once and records the result, so its recorded
/// path is real by construction. Turning that path into a symlink afterwards —
/// to another perfectly valid project — is how the canonical identity moves
/// without the string changing, and it is the case `PreparedOrchestration::path`'s
/// "the canonical path is the string the spawn uses" contract is worthless
/// without.
#[test]
fn a_project_path_that_now_points_elsewhere_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "Prepared against the real directory.");

    let (_other_guard, other) = self::project();
    prepare(&other, "A different project's brief.");

    // Move the real directory aside and put a symlink to the other project in
    // its place, so the recorded path still exists and still resolves — just not
    // to what was prepared.
    let moved = project.with_extension("moved");
    std::fs::rename(&project, &moved).expect("move the real directory aside");
    std::os::unix::fs::symlink(&other, &project).expect("symlink the prepared path elsewhere");

    expect_stale(&binding, PreparationStale::ProjectMoved);

    // Clean up the symlink before the TempDir guard runs, so its recursive
    // delete does not walk into the other project through the link.
    std::fs::remove_file(&project).expect("unlink");
    std::fs::rename(&moved, &project).expect("put the real directory back");
}

/// Scenario: Delete or replace the unique published context with a directory and
/// confirm each prepared start is refused with its corresponding cause.
///
/// A published context that has been deleted, or replaced by something that is
/// not a regular file, is refused rather than read.
///
/// The FIFO half matters beyond tidiness: `revalidate_preparation` runs on a
/// blocking pool thread, and a FIFO with no writer never produces a byte, so
/// reading one would pin that thread rather than answer.
#[test]
fn a_missing_or_non_regular_context_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "Prepared, then vandalised.");

    std::fs::remove_file(context_file(&binding)).expect("delete the published context");
    expect_stale(&binding, PreparationStale::ContextUnreadable);

    std::fs::create_dir(context_file(&binding)).expect("put a directory in its place");
    expect_stale(&binding, PreparationStale::ContextNotRegularFile);
}

/// The control, and the test that stops every assertion above from passing
/// against a checker that simply refuses everything.
///
/// An untouched preparation re-validates as current, and it does so repeatedly:
/// one launch presents the same token once per role, so a check that consumed
/// the record would refuse every role after the first.
#[test]
fn an_untouched_preparation_re_validates_and_stays_re_validatable() {
    let (_guard, project) = project();
    let (token, binding) = prepare(&project, "Nothing happens to this one.");

    for role in 0..3 {
        revalidate_preparation(&binding)
            .unwrap_or_else(|e| panic!("role {role} must still spawn, got {e}"));
        assert_eq!(
            dot_agent_deck::prep_token::binding(&token).as_ref(),
            Some(&binding),
            "presenting a token must not consume it"
        );
    }
}

/// Scenario: Prepare one task and inspect the resulting binding and context
/// bytes to confirm the token identifies exactly what was published.
///
/// What the binding actually records, asserted against the project rather than
/// against the struct's own defaults.
///
/// The original design bound only time; this pins that every field named in the
/// audit's minimum — canonical directory identity, config revision,
/// orchestration, and a digest of the context actually published — is populated
/// from the preparation and not left empty. A binding with an empty digest would
/// make `two_preparations_…` pass for the wrong reason.
#[test]
fn the_binding_records_the_state_the_preparation_approved() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "A brief with a distinctive sentinel: zqx-42.");

    assert_eq!(binding.project_dir, project);
    assert!(
        binding.project_identity.is_some(),
        "the canonical directory's inode identity is what catches a recreate"
    );
    assert!(
        binding.config_revision.starts_with("fnv1a128-"),
        "the config revision must be the resolver's own, got {:?}",
        binding.config_revision
    );
    assert_eq!(binding.orchestration, "loop");
    assert_unique_context_path(&project, &binding.context_path);
    assert!(
        binding.context_identity.is_some(),
        "the published file's inode identity is what catches a republish"
    );
    assert!(
        binding.context_digest.starts_with("ctx-fnv1a128-"),
        "the context digest must be scheme-prefixed and distinct from a config \
         revision, got {:?}",
        binding.context_digest
    );

    // And the digest is of the bytes actually published, not of something
    // adjacent: re-digesting the file off disk reproduces it.
    let on_disk = std::fs::read_to_string(context_file(&binding)).expect("read back");
    assert!(on_disk.contains("zqx-42"));
    assert_eq!(
        dot_agent_deck::project_resolve::context_digest(&on_disk),
        binding.context_digest
    );
}

// ---------------------------------------------------------------------------
// PRD #819, Greptile P1(a): matching the REQUEST against the binding
// ---------------------------------------------------------------------------
//
// Everything above proves that the binding still describes what it approved.
// **Nothing above compares it to what is being asked for**, and for one round
// nothing anywhere did: `revalidate_preparation` checked the record against the
// filesystem and the spawn then used the SUBMITTED `cwd`, `command`, `env` and
// `tab_membership`. So a caller could present a token prepared for project X
// while submitting spawn fields for project Y, and the daemon validated X and
// started Y.
//
// The shape of that miss is worth keeping: the audit fix made the code *look*
// validated, which is exactly what made the second half easy to overlook. These
// cases drive `verify_prepared_start` — the function the daemon's
// `start-prepared-agent` arm actually calls — and each names the specific
// `PreparationMismatch` so a passing assertion shows *which* comparison fired
// rather than only that something refused.

/// The request a launch of `LAUNCHABLE_PROJECT`'s start role legitimately makes.
fn matching_request(project: &Path) -> PreparedStartRequest {
    let cwd = project.to_str().expect("utf-8 project path").to_string();
    PreparedStartRequest {
        cwd: Some(cwd.clone()),
        membership: Some(PreparedStartMembership {
            orchestration: "loop".into(),
            orchestration_cwd: Some(cwd),
            role: "planner".into(),
            is_start_role: true,
        }),
    }
}

fn expect_mismatch(
    binding: &PrepBinding,
    request: &PreparedStartRequest,
    expected: PreparationMismatch,
) {
    match verify_prepared_start(binding, request) {
        Ok(()) => panic!("expected {expected:?}, but the prepared start was accepted"),
        Err(PreparedStartRefusal::Stale(stale)) => panic!(
            "expected the mismatch {expected:?}, got the staleness refusal {stale:?} ({}) — \
             the request comparison must run before, and separately from, the filesystem checks",
            stale.detail()
        ),
        Err(PreparedStartRefusal::Mismatch(actual)) => assert_eq!(
            actual,
            expected,
            "expected {expected:?}, got {actual:?} ({})",
            actual.detail()
        ),
    }
}

/// **The finding itself.** A token prepared for project X, presented with spawn
/// fields naming project Y, is refused — and the refusal says the request does
/// not match the preparation rather than that the preparation went stale, which
/// are different facts with different remedies.
///
/// Both projects are real and both preparations are live, so neither token has
/// aged out and neither artifact has been replaced: every staleness check passes
/// and the ONLY thing wrong is that this request is not the one this token
/// approved. Against the code before this fix the refusal does not happen at all
/// — `revalidate_preparation` returns `Ok` for X's binding and the daemon spawns
/// in Y.
#[test]
fn a_token_prepared_for_one_project_cannot_start_another() {
    let (_guard_x, project_x) = project();
    let (_guard_y, project_y) = project();
    let (_token_x, binding_x) = prepare(&project_x, "Project X's brief.");
    let (_token_y, binding_y) = prepare(&project_y, "Project Y's brief.");

    // Both preparations are intact, which is what makes this a mismatch rather
    // than a staleness case.
    revalidate_preparation(&binding_x).expect("X's preparation is untouched");
    revalidate_preparation(&binding_y).expect("Y's preparation is untouched");

    expect_mismatch(
        &binding_x,
        &matching_request(&project_y),
        PreparationMismatch::ProjectDiffers,
    );

    // The positive control, so "refuses" is not trivially true: X's own request
    // still starts, and so does Y's under Y's token.
    verify_prepared_start(&binding_x, &matching_request(&project_x))
        .expect("the request the preparation approved must still be accepted");
    verify_prepared_start(&binding_y, &matching_request(&project_y))
        .expect("the request the preparation approved must still be accepted");
}

/// A request that sends no working directory at all is not a neutral one: it
/// asks the daemon to spawn in its own cwd, which is not the prepared project.
#[test]
fn a_prepared_start_without_a_working_directory_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "A brief.");

    let mut request = matching_request(&project);
    request.cwd = None;
    expect_mismatch(&binding, &request, PreparationMismatch::ProjectDiffers);
}

/// A preparation approves an **orchestration launch**, so a start that presents
/// its token and declares no orchestration membership is not that launch.
#[test]
fn a_prepared_start_with_no_orchestration_membership_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "A brief.");

    let mut request = matching_request(&project);
    request.membership = None;
    expect_mismatch(&binding, &request, PreparationMismatch::NoOrchestration);
}

/// Same project, different workflow. The path check alone would let this
/// through, which is why the orchestration is bound separately from it.
#[test]
fn a_prepared_start_naming_another_orchestration_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "A brief.");

    let mut request = matching_request(&project);
    if let Some(membership) = request.membership.as_mut() {
        membership.orchestration = "some-other-loop".into();
    }
    expect_mismatch(
        &binding,
        &request,
        PreparationMismatch::OrchestrationDiffers,
    );
}

/// The membership carries a **second** copy of the project identity —
/// `orchestration_cwd`, which is what keys `pane_orchestration_map` — so it is
/// checked against the same prepared directory the `cwd` is.
#[test]
fn a_prepared_start_whose_orchestration_cwd_disagrees_is_refused() {
    let (_guard_other, other) = project();
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "A brief.");

    let mut request = matching_request(&project);
    if let Some(membership) = request.membership.as_mut() {
        membership.orchestration_cwd = Some(other.to_str().expect("utf-8").to_string());
    }
    expect_mismatch(
        &binding,
        &request,
        PreparationMismatch::OrchestrationCwdDiffers,
    );

    // Absent is not the same as wrong: a membership that declares no
    // orchestration cwd claims nothing, and older clients omit the field.
    let mut request = matching_request(&project);
    if let Some(membership) = request.membership.as_mut() {
        membership.orchestration_cwd = None;
    }
    verify_prepared_start(&binding, &request)
        .expect("an absent orchestration cwd asserts nothing and must not refuse");
}

/// The role has to be one the approved orchestration declares, and the role list
/// comes from the config read the staleness checks just passed rather than from
/// a second read of whatever is on disk now.
#[test]
fn a_prepared_start_naming_an_undeclared_role_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "A brief.");

    let mut request = matching_request(&project);
    if let Some(membership) = request.membership.as_mut() {
        membership.role = "a-role-this-orchestration-does-not-have".into();
    }
    expect_mismatch(&binding, &request, PreparationMismatch::RoleNotDeclared);

    // `builder` IS declared, so the refusal above is about the name rather than
    // about the check refusing every non-start role.
    let mut request = matching_request(&project);
    if let Some(membership) = request.membership.as_mut() {
        membership.role = "builder".into();
        membership.is_start_role = false;
    }
    verify_prepared_start(&binding, &request)
        .expect("every declared role of the approved orchestration must still start");
}

/// The start marker is the orchestration's to declare, and it is what puts a
/// pane in `orchestrator_pane_ids` — so a request that marks the wrong role as
/// the start role would make delegations come from somewhere the config never
/// said.
#[test]
fn a_prepared_start_with_the_wrong_start_marker_is_refused() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "A brief.");

    // `planner` is the start role; claiming it is not.
    let mut request = matching_request(&project);
    if let Some(membership) = request.membership.as_mut() {
        membership.is_start_role = false;
    }
    expect_mismatch(&binding, &request, PreparationMismatch::StartMarkerDiffers);

    // `builder` is not the start role; claiming it is.
    let mut request = matching_request(&project);
    if let Some(membership) = request.membership.as_mut() {
        membership.role = "builder".into();
        membership.is_start_role = true;
    }
    expect_mismatch(&binding, &request, PreparationMismatch::StartMarkerDiffers);
}

/// **What is deliberately NOT bound, and this test is the reason to keep it that
/// way.** Per-launch command override is an existing, documented feature —
/// `docs/develop/desktop-gui.md`: "The submitted command overrides the matching
/// project role for that launch only" — and it is how the desktop's agent
/// profiles reach a launch at all.
///
/// The request type therefore carries no command at all, which is what makes
/// "the command is not bound" a property of the seam rather than of an omitted
/// comparison someone could add back. This asserts the shape of the type, which
/// is the only place the property can be observed: a struct with no such field
/// cannot be compared against one.
#[test]
fn the_submitted_command_is_not_part_of_what_a_prepared_start_binds() {
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "A brief.");

    // The config's `planner` command is `cat`; a launch that overrides it — as
    // every desktop profile launch does — presents the same identity and is
    // accepted, because identity is what is bound and content is not.
    verify_prepared_start(&binding, &matching_request(&project))
        .expect("a launch that overrides the role command must still start");

    // And the whole request is the two identity halves, so there is nowhere for
    // a command to be compared even if someone wanted to.
    let request = matching_request(&project);
    assert_eq!(
        request,
        PreparedStartRequest {
            cwd: request.cwd.clone(),
            membership: request.membership.clone(),
        },
        "PreparedStartRequest is the submitted IDENTITY and nothing else"
    );
}

/// Scenario: Rewrite a preparation's context, then submit a request for a
/// different project. The request reports the identity mismatch first, while
/// a matching request reports the rewritten context as stale.
///
/// Ordering: the identity comparisons run first, so a request that does not
/// match its own token is refused with the code that is true of it even when the
/// preparation has ALSO gone stale.
///
/// This matters for diagnosis rather than for safety — both answers refuse — but
/// "your artifact was rewritten" and "you are not asking for what you prepared"
/// send an operator in different directions, which is the same reasoning that
/// separates `stale-token` from `stale-preparation`.
#[test]
fn a_mismatched_request_is_named_as_one_even_when_the_preparation_is_also_stale() {
    let (_guard_other, other) = project();
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "The first brief.");
    // Rewrite this preparation's own unique file in place. Another
    // preparation in the project would leave this binding valid.
    std::fs::write(&binding.context_path, "A different brief.")
        .expect("rewrite the prepared context");
    expect_stale(&binding, PreparationStale::ContextRewritten);

    expect_mismatch(
        &binding,
        &matching_request(&other),
        PreparationMismatch::ProjectDiffers,
    );

    // And a MATCHING request against that same stale binding still reports the
    // staleness, so the ordering above is not swallowing it.
    match verify_prepared_start(&binding, &matching_request(&project)) {
        Err(PreparedStartRefusal::Stale(PreparationStale::ContextRewritten)) => {}
        other => panic!(
            "a matching request against a stale binding must report the staleness, got {other:?}"
        ),
    }
}
