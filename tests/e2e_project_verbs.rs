#![cfg(all(feature = "e2e", unix))]

//! L2 lane-1 coverage for the daemon's three project verbs on the attach
//! socket — `list-projects`, `resolve-project` and `prepare-workflow`
//! (PRD #819).
//!
//! Lane 1 deliberately: every test here drives a headless `daemon serve`
//! against a minted project directory and spends no credential, so CI runs all
//! three on every PR. That matters more than usual for this surface, because
//! the behaviour they pin is the one the desktop stops doing for itself — a
//! client that resolves a project against its OWN filesystem is correct only
//! while the daemon happens to be on the same machine.
//!
//! What these cover that the two non-e2e suites do not:
//!
//!   - `tests/project_projection.rs` pins the wire SHAPE — which keys
//!     `ProjectRole` may carry, that the four new `AttachResponse` fields are
//!     additive, that no config type gained `Serialize`. It never reaches a
//!     daemon.
//!   - `tests/daemon_protocol.rs` pins the wire BOUNDARY — that the three verbs
//!     parse and dispatch, that a malformed path and an over-long task are
//!     refused with a stable code before any filesystem access, and that
//!     `Hello` advertises the capability strings. It stops where the behaviour
//!     starts.
//!
//! These three are the behaviour: what the enumeration is allowed to offer,
//! where the coordinator context is published and what a failed preparation
//! must not do, and that one canonical path string carries from the resolve
//! through to the launch.
//!
//! `unix` on the file gate because the whole thing binds and drives
//! Unix-domain sockets (`DaemonProc`) and `project/launch/002` builds a
//! symlink; the same pattern as `tests/e2e_remote_doctor.rs` and
//! `tests/e2e_tab_close_regressions.rs`. All polling lives in `common` helpers
//! so these bodies carry no raw sleep (linkage-check Decision 21).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::DaemonProc;
use dot_agent_deck::daemon_protocol::{AttachRequest, PROJECT_ERR_UNIMPLEMENTED};
use spec::spec;

/// A project whose single orchestration is NAMED, so nothing about it depends
/// on the directory it sits in. The start role carries a `prompt_template` and
/// the worker role a `description`, because those are the two config fields the
/// composed coordinator context is asserted to contain — the content check
/// `desktop/src-tauri/src/lib.rs`'s `workflow_launch_prepares_canonical_context_in_config_order`
/// provides today and loses when the write moves daemon-side.
const NAMED_PROJECT_TOML: &str = r#"
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

/// The same project with the orchestration's `name` key OMITTED. `name` is
/// `#[serde(default)]` on `RawOrchestration`, and `load_project_config`
/// normalises an empty one to the loaded directory's basename
/// (`resolve_orchestration_name`, `src/project_config.rs`). That is what makes
/// `project/launch/002` able to see WHICH spelling of the path the daemon
/// resolved against.
const UNNAMED_PROJECT_TOML: &str = r#"
[[orchestrations]]

[[orchestrations.roles]]
name = "planner"
command = "cat"
start = true

[[orchestrations.roles]]
name = "builder"
command = "cat"
description = "Implements the requested change"
"#;

/// Create `parent/name`, and drop a `.dot-agent-deck.toml` in it when `config`
/// is `Some`. `None` makes an ordinary directory — a perfectly legitimate agent
/// cwd, and not a project.
fn make_dir(parent: &Path, name: &str, config: Option<&str>) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    if let Some(toml) = config {
        std::fs::write(dir.join(".dot-agent-deck.toml"), toml)
            .unwrap_or_else(|e| panic!("seed .dot-agent-deck.toml in {}: {e}", dir.display()));
    }
    dir
}

/// The canonical spelling of `path`. Every assertion compares against this
/// rather than against the path the test constructed: the harness temp root can
/// itself sit behind a symlink (`/var` → `/private/var` on macOS), so the raw
/// spelling is not necessarily the canonical one and a test that assumed
/// otherwise would fail for a reason that has nothing to do with the verb.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()))
}

/// `path` as the `String` the wire carries.
fn wire_path(path: &Path) -> String {
    path.to_str()
        .unwrap_or_else(|| panic!("harness paths are UTF-8: {}", path.display()))
        .to_string()
}

/// Register one long-lived synthetic agent with `cwd` recorded on its
/// `AgentRecord`, which is the enumeration seed PRD #819 draws on. No LLM and
/// no credential — a `sleep` stub is enough, because the claim under test is
/// about the daemon's own view of where its agents are.
fn start_seed_agent(daemon: &DaemonProc, label: &str, cwd: &Path) {
    let resp = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some("sh -c 'sleep 600'".into()),
            cwd: Some(wire_path(cwd)),
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), format!("pane-{label}"))],
            display_name: Some(label.to_string()),
            tab_membership: None,
            agent_type: None,
            seed: None,
        })
        .expect("StartAgent over the attach socket");
    assert!(
        resp.error.is_none(),
        "seeding an agent in {} failed: {:?}",
        cwd.display(),
        resp.error
    );
}

/// Scenario: Start a headless daemon and give it two live agents — one whose
/// working directory is a bare directory holding no `.dot-agent-deck.toml`, one
/// whose working directory is a real project — then ask it over the attach
/// socket to list the projects it knows about. The reply must offer the real
/// project's canonical path and must NOT offer the bare directory, because an
/// agent cwd is a candidate rather than proof that a project lives there.
#[spec("project/resolve/001")]
#[test]
fn project_resolve_001_enumeration_offers_only_projects_that_resolve() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    let workspace = common::harness_tempdir().expect("mint the project sandbox");
    let bare = make_dir(workspace.path(), "bare-agent-cwd", None);
    let project = make_dir(workspace.path(), "real-project", Some(NAMED_PROJECT_TOML));

    start_seed_agent(&daemon, "bare-seed", &bare);
    start_seed_agent(&daemon, "project-seed", &project);
    let records = daemon.wait_for_agent_count(2, Duration::from_secs(10));
    assert_eq!(
        records.len(),
        2,
        "both seed agents must be registered before the enumeration is asked for"
    );

    let resp = daemon
        .send_attach_request(&AttachRequest::ListProjects {})
        .expect("ListProjects over the attach socket");
    assert!(
        resp.ok,
        "ListProjects must enumerate the projects this daemon knows about; it refused instead: {:?}",
        resp.error
    );
    let listing = resp
        .projects
        .expect("a successful ListProjects must carry a ProjectListing, not an absent field");

    let want = canonical(&project);
    let unwanted = canonical(&bare);
    let offered: Vec<&str> = listing.projects.iter().map(|p| p.path.as_str()).collect();

    assert!(
        offered.iter().any(|p| Path::new(p) == want),
        "the enumeration must offer the project at {}; offered {offered:?}",
        want.display()
    );
    assert!(
        !offered.iter().any(|p| Path::new(p) == unwanted),
        "a bare agent cwd holding no .dot-agent-deck.toml must not be offered as a project, \
         but {} is in the listing; offered {offered:?}",
        unwanted.display()
    );
    if let Some(primary) = listing.primary.as_deref() {
        assert!(
            offered.contains(&primary),
            "`primary` must nominate one of the offered projects, but {primary:?} is not among {offered:?}"
        );
    }
}

/// Scenario: Start a headless daemon and ask it to prepare a workflow against a
/// real project, then check both halves of the answer — the coordinator context
/// is published at `<project>/.dot-agent-deck/orchestrator-context.md` carrying
/// the configured prompt template, the worker's description and the task, and
/// the reply's own `context_path` names that same file. Then ask it to prepare
/// an orchestration the project does not define, and check that the refusal
/// publishes no context and leaves the daemon holding zero panes.
#[spec("project/launch/001")]
#[test]
fn project_launch_001_publishes_the_context_and_a_failed_preparation_starts_no_roles() {
    const TASK: &str = "Build the requested feature.";

    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    let workspace = common::harness_tempdir().expect("mint the project sandbox");
    let project = canonical(&make_dir(
        workspace.path(),
        "prepared-project",
        Some(NAMED_PROJECT_TOML),
    ));
    let doomed = canonical(&make_dir(
        workspace.path(),
        "unlaunchable-project",
        Some(NAMED_PROJECT_TOML),
    ));

    assert!(
        daemon.agent_records().is_empty(),
        "precondition: the daemon must hold no panes before either preparation"
    );

    // --- the preparation that must succeed: assert the reply AND the side
    // effect. A side-effect-only check cannot cover what the verb returns, and
    // a reply-only check cannot cover where the agent will actually read.
    let resp = daemon
        .send_attach_request(&AttachRequest::PrepareWorkflow {
            path: wire_path(&project),
            orchestration: "loop".into(),
            task: TASK.into(),
            config_revision: None,
        })
        .expect("PrepareWorkflow over the attach socket");
    assert!(
        resp.ok,
        "PrepareWorkflow must resolve the project, compose the coordinator context and publish \
         it; it refused instead: {:?}",
        resp.error
    );
    let prepared = resp
        .workflow_prepared
        .expect("a successful PrepareWorkflow must carry a PreparedWorkflow");

    let expected_context = project
        .join(".dot-agent-deck")
        .join("orchestrator-context.md");
    assert_eq!(
        Path::new(&prepared.context_path),
        expected_context,
        "the reply must name the file the agent will actually read"
    );
    assert!(
        expected_context.is_file(),
        "the coordinator context must already exist at {} when PrepareWorkflow reports success — \
         the publish happens before the reply, not after it",
        expected_context.display()
    );

    let context = std::fs::read_to_string(&expected_context)
        .unwrap_or_else(|e| panic!("read {}: {e}", expected_context.display()));
    // Name what is missing rather than dumping the file. The coordinator
    // context carries the task, a repository-supplied prompt template and the
    // role descriptions, and PRD #819 declines to assume that content is public
    // merely because guidance discourages secrets in it.
    for needle in [
        "Coordinate through the configured team.",
        "**builder**: Implements the requested change",
        "## Delegation protocol",
        "## Your task",
        TASK,
    ] {
        assert!(
            context.contains(needle),
            "the published coordinator context ({} bytes at {}) is missing {needle:?}",
            context.len(),
            expected_context.display()
        );
    }

    let roles: Vec<(&str, bool)> = prepared
        .roles
        .iter()
        .map(|r| (r.name.as_str(), r.start))
        .collect();
    assert!(
        roles.contains(&("planner", true)),
        "the reply must carry the start role the client orders its spawn from; got {roles:?}"
    );
    assert!(
        roles.contains(&("builder", false)),
        "the reply must carry the non-start role; got {roles:?}"
    );

    // --- the preparation that must fail: nothing published, nothing started.
    let doomed_context = doomed
        .join(".dot-agent-deck")
        .join("orchestrator-context.md");
    let resp = daemon
        .send_attach_request(&AttachRequest::PrepareWorkflow {
            path: wire_path(&doomed),
            orchestration: "no-such-orchestration".into(),
            task: "This preparation must fail.".into(),
            config_revision: None,
        })
        .expect("PrepareWorkflow over the attach socket");
    assert!(
        !resp.ok,
        "naming an orchestration the project does not define must be refused, not accepted"
    );
    assert!(
        resp.workflow_prepared.is_none(),
        "a refused preparation must carry no PreparedWorkflow"
    );
    let error = resp.error.unwrap_or_default();
    assert!(
        !error.starts_with(PROJECT_ERR_UNIMPLEMENTED),
        "the refusal must come from resolving the project and failing to find the orchestration, \
         not from the verb having no implementation behind it: {error:?}"
    );
    assert!(
        !doomed_context.exists(),
        "a failed preparation must publish nothing, but {} was written",
        doomed_context.display()
    );
    let records = daemon.agent_records();
    let started: Vec<Option<String>> = records.iter().map(|r| r.display_name.clone()).collect();
    assert!(
        records.is_empty(),
        "a failed preparation must start no roles, but the daemon holds {} pane(s): {started:?}",
        records.len()
    );
}

/// Scenario: Create a real project directory plus a symlink pointing at it,
/// then resolve the project through the SYMLINKED spelling. The daemon must
/// answer with the canonical path, must name the unnamed orchestration after
/// the canonical basename rather than the symlink's, and a launch prepared
/// against the path it returned must publish its coordinator context under that
/// same canonical directory — so the listing and the spawn can never name two
/// different things.
#[spec("project/launch/002")]
#[test]
fn project_launch_002_the_canonical_path_resolve_returns_is_the_string_the_launch_uses() {
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    let workspace = common::harness_tempdir().expect("mint the project sandbox");
    let root = canonical(workspace.path());
    let code = make_dir(&root, "code", None);
    let project = canonical(&make_dir(
        &code,
        "canonical-project",
        Some(UNNAMED_PROJECT_TOML),
    ));

    // The alias differs from the project in its BASENAME, which is the whole
    // point: an unnamed orchestration is named after the directory basename, so
    // resolving through `current` and canonicalising only partway through the
    // flow makes the listing say one name and the spawn say another — PRD
    // #220's bug verbatim (`src/dispatch.rs`).
    let alias = root.join("current");
    std::os::unix::fs::symlink(&project, &alias)
        .unwrap_or_else(|e| panic!("symlink {} -> {}: {e}", alias.display(), project.display()));

    let resp = daemon
        .send_attach_request(&AttachRequest::ResolveProject {
            path: wire_path(&alias),
        })
        .expect("ResolveProject over the attach socket");
    assert!(
        resp.ok,
        "ResolveProject must resolve a symlinked spelling; it refused instead: {:?}",
        resp.error
    );
    let resolved = resp
        .project
        .expect("a successful ResolveProject must carry a ResolvedProject");

    assert_eq!(
        Path::new(&resolved.path),
        project,
        "the daemon must answer with the canonical path, not the {} spelling it was sent",
        alias.display()
    );

    let names: Vec<&str> = resolved
        .orchestrations
        .iter()
        .map(|o| o.name.as_str())
        .collect();
    assert!(
        names.contains(&"canonical-project"),
        "an unnamed orchestration is named after the project directory's basename, and \
         canonicalising a symlinked path CHANGES that basename — the resolve must say \
         `canonical-project`; got {names:?}"
    );
    assert!(
        !names.contains(&"current"),
        "the symlink's basename must never name an orchestration; got {names:?}"
    );

    // The second half of the same claim: the string the resolve returned is the
    // string the launch uses, so the context lands under the canonical
    // directory and the reply says so.
    let resp = daemon
        .send_attach_request(&AttachRequest::PrepareWorkflow {
            path: resolved.path.clone(),
            orchestration: "canonical-project".into(),
            task: "Prove the canonical spelling survives the whole flow.".into(),
            config_revision: None,
        })
        .expect("PrepareWorkflow over the attach socket");
    assert!(
        resp.ok,
        "a launch prepared against the path ResolveProject returned must succeed; it refused: {:?}",
        resp.error
    );
    let prepared = resp
        .workflow_prepared
        .expect("a successful PrepareWorkflow must carry a PreparedWorkflow");

    let expected_context = project
        .join(".dot-agent-deck")
        .join("orchestrator-context.md");
    assert_eq!(
        Path::new(&prepared.context_path),
        expected_context,
        "the launch must publish under the canonical project directory the resolve named — a \
         context reported through the {} spelling is how the listing and the spawn drift apart",
        alias.display()
    );
    assert!(
        expected_context.is_file(),
        "the coordinator context must exist at {}",
        expected_context.display()
    );
}
