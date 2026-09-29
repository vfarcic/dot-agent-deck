//! The project directory a prepared start's staleness checks verified is handed
//! back **held open**, so the spawn can enter the object that was checked
//! (issue #1233 item 2).
//!
//! Before this, `verify_prepared_start_role` read the directory's identity with a
//! pathname `symlink_metadata`, returned only the matched role, and the spawn
//! then entered the directory by `cmd.cwd(path)` — a second pathname lookup with
//! the whole config and context re-validation in between. These pin the checker
//! half of the fix: the handle it returns is the directory the binding approved,
//! and a replacement is still refused as `ProjectReplaced` through the new open.
//! That the spawn actually enters the held object is
//! `spawn_in_enters_the_verified_directory_even_after_its_path_is_replaced` in
//! `src/agent_pty.rs`.
//!
//! **Fast tier, and deliberately not linked against `tests/common/`** — same
//! reasoning as `tests/prep_binding.rs`: `#[path]`-include the self-contained
//! `src/test_temp.rs` rather than pull the whole PTY harness into another binary.

// The handle is a `cfg(unix)` type: the verbs that mint a prepared start are
// refused on every other platform (`PROJECT_ERR_UNSUPPORTED_PLATFORM`).
#![cfg(unix)]

use std::os::fd::{AsFd as _, AsRawFd as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use dot_agent_deck::prep_token::{InodeIdentity, PrepBinding};
use dot_agent_deck::project_resolve::{
    PreparationStale, PreparedStartMembership, PreparedStartRefusal, PreparedStartRequest,
    prepare_orchestration_for_wire, verify_prepared_start_role,
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
"#;

fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = test_temp::tempdir().expect("mint the project sandbox");
    std::fs::write(dir.path().join(".dot-agent-deck.toml"), LAUNCHABLE_PROJECT)
        .expect("seed the project config");
    let canonical = std::fs::canonicalize(dir.path()).expect("canonicalize the project sandbox");
    (dir, canonical)
}

/// Through the real launch verb, not a hand-built binding, so the identity the
/// handle is compared against is the one a preparation actually records.
fn prepare(project: &Path) -> PrepBinding {
    let prepared = prepare_orchestration_for_wire(
        project.to_str().expect("utf-8 project path"),
        "loop",
        "A brief.",
        None,
        &[],
    )
    .expect("the preparation must succeed");
    dot_agent_deck::prep_token::binding(&prepared.token)
        .expect("a freshly issued token must resolve to its binding")
}

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

/// The handle `verify_prepared_start_role` returns is the directory the binding
/// approved: its `fstat` — read from the descriptor itself, not from the path —
/// equals `binding.project_identity`. The descriptor also carries the two
/// properties the Linux spawn path depends on: it is close-on-exec, so it never
/// reaches the agent, and it is above 2, so the child's `dup2` of the PTY onto
/// stdio cannot shadow it before the `chdir`.
#[test]
fn verify_prepared_start_role_returns_a_handle_on_the_approved_directory() {
    let (_guard, project) = project();
    let binding = prepare(&project);

    let verified = verify_prepared_start_role(&binding, &matching_request(&project))
        .expect("an untouched preparation must verify");
    assert_eq!(verified.role.name, "planner");

    let fd = verified.project_dir.as_fd();
    let metadata = std::fs::File::from(fd.try_clone_to_owned().expect("dup the handle"))
        .metadata()
        .expect("fstat the handle");
    assert!(metadata.is_dir());
    let fstat = InodeIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    };
    assert_eq!(Some(fstat), binding.project_identity);
    assert_eq!(verified.project_dir.identity(), fstat);

    assert!(fd.as_raw_fd() > 2, "the handle must sit above stdio");
    // SAFETY: F_GETFD on a descriptor this test borrows for the call.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD failed");
    assert_ne!(
        flags & libc::FD_CLOEXEC,
        0,
        "the handle must be close-on-exec"
    );
}

/// A directory deleted and recreated under the prepared name before the check
/// runs is refused as `ProjectReplaced` — the finding the pathname
/// `symlink_metadata` comparison reached, now reached through the descriptor
/// open. The identity check runs before the config and context checks, so the
/// empty replacement is refused for its identity and not for a missing file.
#[test]
fn a_directory_replaced_before_the_open_is_still_refused_as_replaced() {
    let (_guard, project) = project();
    let binding = prepare(&project);

    std::fs::rename(&project, project.with_extension("moved-aside"))
        .expect("move the original directory aside");
    std::fs::create_dir(&project).expect("build a different directory under the same name");

    match verify_prepared_start_role(&binding, &matching_request(&project)) {
        Err(PreparedStartRefusal::Stale(PreparationStale::ProjectReplaced)) => {}
        other => panic!("a replaced project directory must be refused, got {other:?}"),
    }
}
