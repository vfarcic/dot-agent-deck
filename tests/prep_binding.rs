//! What a preparation token binds, and what a spawn presenting it re-validates
//! (PRD #819, the audit fix on the finished branch).
//!
//! **The defect these tests exist for is a design defect, not an implementation
//! slip.** `PrepareWorkflow` publishes the coordinator context to a path that is
//! **fixed per project** — `<project>/.dot-agent-deck/orchestrator-context.md` —
//! and the original design issued a token recording only `(token, issuance
//! time)`. The spawn then validated only that the token existed and was younger
//! than the TTL. So two ordinary clients preparing in the same project
//! interleaved, with **no attacker required**:
//!
//! 1. preparation A publishes context A and receives token A;
//! 2. preparation B replaces the same fixed file with context B;
//! 3. token A is still valid, so A's spawn launches a coordinator whose prompt
//!    names that fixed path — and it reads **context B**.
//!
//! Deleting and recreating the project directory, or changing its config after
//! the preparation, is the same class of mismatch. Every test below fails against
//! a time-only token — none of them can even be *expressed* against one, since
//! there is nothing to inspect — and each isolates one of the five checks
//! `project_resolve::revalidate_preparation` performs, naming it through the
//! daemon-local `PreparationStale` so a passing assertion shows *which* check
//! fired rather than only that something refused.
//!
//! **The fixed path is kept, deliberately, and the disposition is stated rather
//! than left to "last writer wins".** A per-launch or content-addressed filename
//! was the alternative; it was not taken because the file name is named in the
//! agent-facing prompt line, in `read_back_task`'s re-assertion path and in
//! guidance across this repository, so moving it is a far larger change than the
//! window it closes. Instead: the **second** preparation's context is the one on
//! disk and the one its own token validates against, and the **first**
//! preparation is refused at its spawn rather than launched against the wrong
//! brief. `two_preparations_in_one_project_refuse_the_first_and_accept_the_second`
//! pins exactly that pair.
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

use dot_agent_deck::orchestrator_context::{CONTEXT_DIR_NAME, CONTEXT_FILE_NAME};
use dot_agent_deck::prep_token::PrepBinding;
use dot_agent_deck::project_resolve::{
    PreparationMismatch, PreparationStale, PreparedStartMembership, PreparedStartRefusal,
    PreparedStartRequest, prepare_workflow_for_wire, revalidate_preparation, verify_prepared_start,
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

fn context_file(project: &Path) -> PathBuf {
    project.join(CONTEXT_DIR_NAME).join(CONTEXT_FILE_NAME)
}

/// Prepare a workflow in `project` and hand back the binding its token carries.
///
/// Going through `prepare_workflow_for_wire` rather than constructing a
/// `PrepBinding` by hand is the point: a hand-built binding would prove the
/// checker consistent with the test author's idea of a preparation, not with
/// what the launch verb actually records.
fn prepare(project: &Path, task: &str) -> (String, PrepBinding) {
    let prepared = prepare_workflow_for_wire(
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

/// The finding's core: two preparations in one project, and the first is refused
/// at its spawn while the second still launches.
///
/// A time-only token cannot see any of this — both tokens are seconds old and
/// well inside the TTL — which is why the fix had to be a *binding* rather than
/// a shorter TTL. The `ContextReplaced` cause is the structural one: the publish
/// is a `create_new` temp file plus `rename(2)`, and a rename always installs a
/// fresh inode over the destination, so the second publish is detectable
/// whatever its bytes happen to hash to.
///
/// The last two assertions are the "what happens to the second preparation's
/// file" half of the disposition, pinned rather than left implicit: B's context
/// is the one on disk, and B's token is the one that validates.
#[test]
fn two_preparations_in_one_project_refuse_the_first_and_accept_the_second() {
    let (_guard, project) = project();

    let (_token_a, binding_a) = prepare(&project, "Task A: the first client's brief.");
    let (_token_b, binding_b) = prepare(&project, "Task B: the second client's brief.");

    // Both preparations named the same fixed path, which is the whole shape of
    // the defect.
    assert_eq!(binding_a.context_path, binding_b.context_path);
    assert_eq!(binding_a.context_path, context_file(&project));
    assert_ne!(
        binding_a.context_digest, binding_b.context_digest,
        "two different briefs must digest differently, or this test proves nothing"
    );

    expect_stale(&binding_a, PreparationStale::ContextReplaced);

    revalidate_preparation(&binding_b)
        .expect("the preparation whose artifact is actually on disk must still launch");

    let on_disk = std::fs::read_to_string(context_file(&project)).expect("read the context back");
    assert!(
        on_disk.contains("Task B: the second client's brief."),
        "the second preparation's context is the one on disk"
    );
    assert!(
        !on_disk.contains("Task A: the first client's brief."),
        "and the first's is gone, which is exactly why its token must be refused"
    );
}

/// The same fixed path, rewritten **in place** rather than republished.
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

    let before = std::fs::metadata(context_file(&project)).expect("stat before");
    std::fs::write(context_file(&project), "Follow these instructions instead.")
        .expect("rewrite the published context in place");
    let after = std::fs::metadata(context_file(&project)).expect("stat after");
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
    std::fs::copy(context_file(&moved), context_file(&project)).expect("carry the context forward");

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
/// without the string changing, and it is the case `PreparedWorkflow::path`'s
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

    std::fs::remove_file(context_file(&project)).expect("delete the published context");
    expect_stale(&binding, PreparationStale::ContextUnreadable);

    std::fs::create_dir(context_file(&project)).expect("put a directory in its place");
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
    assert_eq!(binding.context_path, context_file(&project));
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
    let on_disk = std::fs::read_to_string(context_file(&project)).expect("read back");
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

/// Ordering: the identity comparisons run first, so a request that does not
/// match its own token is refused with the code that is true of it even when the
/// preparation has ALSO gone stale.
///
/// This matters for diagnosis rather than for safety — both answers refuse — but
/// "your artifact was replaced" and "you are not asking for what you prepared"
/// send an operator in different directions, which is the same reasoning that
/// separates `stale-token` from `stale-preparation`.
#[test]
fn a_mismatched_request_is_named_as_one_even_when_the_preparation_is_also_stale() {
    let (_guard_other, other) = project();
    let (_guard, project) = project();
    let (_token, binding) = prepare(&project, "The first brief.");
    // A second preparation replaces the context at its fixed path, so the first
    // binding is now stale as well.
    let (_token2, _binding2) = prepare(&project, "The second brief.");
    expect_stale(&binding, PreparationStale::ContextReplaced);

    expect_mismatch(
        &binding,
        &matching_request(&other),
        PreparationMismatch::ProjectDiffers,
    );

    // And a MATCHING request against that same stale binding still reports the
    // staleness, so the ordering above is not swallowing it.
    match verify_prepared_start(&binding, &matching_request(&project)) {
        Err(PreparedStartRefusal::Stale(PreparationStale::ContextReplaced)) => {}
        other => panic!(
            "a matching request against a stale binding must report the staleness, got {other:?}"
        ),
    }
}
