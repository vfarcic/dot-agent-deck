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
use dot_agent_deck::daemon_protocol::{AttachRequest, PROJECT_ERR_UNIMPLEMENTED, TabMembership};
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

/// The process's working directory as it was when this was taken, restored on
/// drop.
///
/// `project/resolve/002` moves the process cwd, which is process-global, and
/// its own doc records why that is sound: both e2e aliases run under nextest,
/// which is process-per-test. This guard is the belt-and-braces half — the
/// restore happens on an unwind as well as on the return, so "the cwd is put
/// back" stops being a property of the *runner* and becomes a property of the
/// test. Under nextest the process is about to exit either way; under anything
/// else a mid-test assertion failure no longer leaves the directory moved for
/// whatever runs next.
///
/// Same name and same shape as the guards `tests/durable_hook_binary_path.rs`,
/// `tests/features.rs` and `src/opencode_manage.rs` each already keep for their
/// own cwd moves — a per-file struct rather than a `common` helper, which is
/// how those three do it too. Each `tests/*.rs` file is its own crate, so a
/// shared one would have to live in `common`, and a shared affordance for
/// moving the process cwd is an invitation rather than a convenience.
struct CwdGuard(PathBuf);

impl CwdGuard {
    fn take() -> Self {
        Self(std::env::current_dir().expect("the test process has a working directory"))
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        // Best-effort by construction: this runs on the unwind path too, where
        // panicking again would abort the process and replace a legible test
        // failure with one that says nothing.
        let _ = std::env::set_current_dir(&self.0);
    }
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
            authoring_kind: None,
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

/// Scenario: Make two directories that are equally good projects, start the
/// daemon standing in one of them, then move the client process into the other
/// so the two sides no longer share a resolution input, and ask the daemon what
/// projects it knows. The listing must name the DAEMON's directory and must not
/// name the client's — an answer a client resolving against its own environment
/// could not have produced.
///
/// # Why this retires a claim, and what the claim got right
///
/// `docs/develop/desktop-gui.md` says a loopback ssh tunnel is not a substitute
/// for a second machine "because locally the client's filesystem *is* the
/// daemon's and every path assertion passes whichever side resolved it". That
/// is true of a naive test and it is **not** a proof that the property is
/// untestable on one machine: what has to differ is not the filesystem but the
/// **resolution inputs**. Two directories on one disk, and a daemon and a
/// client standing in different ones, are distinguishable — and that is all
/// PRD #819's "the client holds no project state" needs to be falsifiable.
///
/// # `HOME` is not the lever, and finding that out is part of the answer
///
/// The obvious divergence to reach for is `HOME`, and it does not work here:
/// **no project-resolution path reads it.** `ResolveProject` refuses anything
/// that is not already absolute (`validate_project_path`, and
/// `tests/daemon_protocol.rs` pins the refusal), so there is no `~` to expand
/// and no relative spelling to resolve against anybody's environment;
/// `read_project_config` reads `<dir>/.dot-agent-deck.toml` and does not walk
/// upwards; and `ListProjects` enumerates from state the daemon already holds.
///
/// The tell is asserted below rather than argued: the harness already gives
/// every daemon its own `HOME`, so if `HOME` were a resolution input this
/// property would have been "provable" by every e2e test in the tree since the
/// harness was written — which is exactly the shape of a test that proves
/// nothing. What actually separates the two sides is the **cwd**, because that
/// is the one environment fact `ListProjects` genuinely resolves from
/// (`capture_daemon_startup_cwd`, taken once in `run_daemon_with` so the answer
/// cannot depend on when it was asked).
///
/// `set_current_dir` is process-global, and that is sound here for the reason
/// it is sound nowhere else: both e2e aliases run under nextest, which is
/// process-per-test, so this process is this test. The original is restored at
/// the end anyway — by a [`CwdGuard`] rather than by a trailing statement,
/// so an assertion that fires mid-test restores it on the way out too. That is
/// belt and braces on top of the nextest argument, not a replacement for it:
/// under nextest the process is about to die either way, and what the guard
/// buys is that the property stops depending on which runner is driving.
#[spec("project/resolve/002")]
#[test]
fn project_resolve_002_the_enumeration_is_the_daemons_cwd_and_not_the_clients() {
    let workspace = common::harness_tempdir().expect("mint the project sandbox");
    // Byte-identical config in both, so nothing about the CONTENT can be what
    // separates them in the listing below.
    let daemon_side = make_dir(workspace.path(), "daemon-side", Some(NAMED_PROJECT_TOML));
    let client_side = make_dir(workspace.path(), "client-side", Some(NAMED_PROJECT_TOML));

    // Taken BEFORE the first move and dropped after the last assertion, so
    // every exit from here on — the return below, or an unwind out of any
    // `assert!` between here and it — puts the process back where it started.
    let _cwd = CwdGuard::take();
    // The daemon inherits this process's cwd, which is what
    // `capture_daemon_startup_cwd` records at startup.
    std::env::set_current_dir(&daemon_side).expect("stand in the daemon's project");
    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    // ...and from here on the client stands somewhere else entirely. The two
    // sides now disagree about "where am I" while sharing one filesystem.
    std::env::set_current_dir(&client_side).expect("stand the client in its own project");

    // The `HOME` half, asserted so the paragraph above is a measurement rather
    // than a claim: the two sides already diverge on `HOME`, and it makes no
    // difference to anything below.
    let client_home = std::env::var("HOME").expect("HOME is set on the host");
    assert_ne!(
        Path::new(&client_home),
        daemon.home.as_path(),
        "the harness already gives the daemon its own HOME — if HOME were a \
         resolution input, this property would have been free all along"
    );

    // Both directories really are projects this daemon can resolve, asked of
    // the daemon itself. Without this the assertion below could pass because
    // the client's directory was never resolvable in the first place, which
    // would prove nothing about WHO resolved.
    for (label, dir) in [
        ("the daemon's", &daemon_side),
        ("the client's", &client_side),
    ] {
        let resp = daemon
            .send_attach_request(&AttachRequest::ResolveProject {
                path: wire_path(&canonical(dir)),
            })
            .expect("ResolveProject over the attach socket");
        assert!(
            resp.ok,
            "{label} directory must be a project this daemon can resolve when \
             asked by absolute path, or the enumeration below proves nothing: \
             {:?}",
            resp.error
        );
    }

    let resp = daemon
        .send_attach_request(&AttachRequest::ListProjects {})
        .expect("ListProjects over the attach socket");
    assert!(
        resp.ok,
        "ListProjects must answer; it refused instead: {:?}",
        resp.error
    );
    let listing = resp
        .projects
        .expect("a successful ListProjects must carry a ProjectListing");
    let offered: Vec<&str> = listing.projects.iter().map(|p| p.path.as_str()).collect();

    let want = canonical(&daemon_side);
    let unwanted = canonical(&client_side);
    assert!(
        offered.iter().any(|p| Path::new(p) == want),
        "the enumeration must name the directory the DAEMON started in ({}); \
         offered {offered:?}",
        want.display()
    );
    assert!(
        !offered.iter().any(|p| Path::new(p) == unwanted),
        "the enumeration must NOT name the directory the CLIENT stands in \
         ({}) — that is the answer a client resolving against its own \
         environment would produce, and on one filesystem it is just as valid a \
         project; offered {offered:?}",
        unwanted.display()
    );
    // `primary` is deliberately absent, and asserting that is what keeps the
    // claim above honest: it is a fact about LIVE state, nominated from the most
    // recent recorded activity, and the daemon's own startup cwd is a seed with
    // no clock (`ProjectCandidate::activity_ms` is `None` for it). So "the
    // daemon knows where it is standing" and "the daemon has something live
    // there" are different answers, and only the first is being made here.
    assert_eq!(
        listing.primary, None,
        "a startup-cwd seed carries no activity timestamp, so nothing may be \
         nominated as primary from it alone; got {:?}",
        listing.primary
    );
}

/// Scenario: Start a headless daemon and ask it to prepare a workflow against a
/// real project, then check both halves of the answer — the coordinator context
/// is published at `<project>/.dot-agent-deck/orchestrator-context.md` carrying
/// the configured prompt template, the worker's description, the task and the
/// `## Task precedence` statement but NOT the unattended notice (this verb's
/// caller is the desktop's launch panel, where the person who typed the task is
/// watching), and the reply's own `context_path` names that same file. Then ask
/// it to prepare an orchestration the project does not define, and check that
/// the refusal publishes no context and leaves the daemon holding zero panes.
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
        // Issue #703: two documents reach the coordinator in one file and only
        // their ORDERING used to say which wins. A task that says to open a PR
        // and stop against a template step that releases both landed on the
        // same place by luck; a template step that said "merge" would have
        // overridden the task's stop condition silently.
        "## Task precedence",
    ] {
        assert!(
            context.contains(needle),
            "the published coordinator context ({} bytes at {}) is missing {needle:?}",
            context.len(),
            expected_context.display()
        );
    }
    // The other half of #703, and the reason the attendance is a caller
    // declaration rather than an inference from "a task was supplied": this
    // verb's caller is the desktop's live-loop panel, which will not launch
    // without a task prompt AND has the operator who typed it watching the
    // panes. Inferring unattendedness from the task would tell that run nobody
    // was there and strip the one gate its operator was present to answer.
    assert!(
        !context.contains("## Unattended run"),
        "a prepared workflow's operator is watching it; the published context ({} bytes at {}) \
         must not tell the coordinator otherwise",
        context.len(),
        expected_context.display()
    );

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

/// Scenario: Prepare the same two-role workflow first with an empty task and
/// then with a real task, checking that only the latter publishes the complete
/// task section. Start both roles from the empty-task preparation with one run
/// title and verify each daemon record preserves that title in its membership.
#[spec("project/launch/004")]
#[test]
fn project_launch_004_empty_task_omits_the_task_section_and_preserves_the_run_title() {
    const RUN_TITLE: &str = "Desktop named run";
    const CONTROL_TASK: &str = "Carry the non-empty control task.";

    let daemon = common::spawn_daemon_serve_with_env(None, "0", &[]);
    let workspace = common::harness_tempdir().expect("mint the project sandbox");
    let project = canonical(&make_dir(
        workspace.path(),
        "empty-task-project",
        Some(NAMED_PROJECT_TOML),
    ));
    let project_wire = wire_path(&project);

    let empty_response = daemon
        .send_attach_request(&AttachRequest::PrepareWorkflow {
            path: project_wire.clone(),
            orchestration: "loop".into(),
            task: String::new(),
            config_revision: None,
        })
        .expect("empty-task PrepareWorkflow over the attach socket");
    assert!(
        empty_response.ok,
        "PrepareWorkflow must accept an empty task; it refused instead: {:?}",
        empty_response.error
    );
    let empty_prepared = empty_response
        .workflow_prepared
        .expect("a successful empty-task preparation must carry a PreparedWorkflow");
    let context_path = PathBuf::from(&empty_prepared.context_path);
    let empty_context = std::fs::read_to_string(&context_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", context_path.display()));

    for needle in [
        "Coordinate through the configured team.",
        "**builder**: Implements the requested change",
        "## Delegation protocol",
    ] {
        assert!(
            empty_context.contains(needle),
            "an empty task must not erase the rest of the coordinator context; missing \
             {needle:?} from {} bytes at {}",
            empty_context.len(),
            context_path.display()
        );
    }
    for forbidden in [
        "\n## Your task\n",
        "\n## Task precedence\n",
        "Everything from `## Your task` to the end of this file is the task",
    ] {
        assert!(
            !empty_context.contains(forbidden),
            "an empty task must omit the whole task section and its boilerplate, but \
             {forbidden:?} appeared in the published context at {}",
            context_path.display()
        );
    }
    assert!(
        !empty_prepared.prompt.contains("## Your task")
            && empty_prepared.prompt.contains("wait for instructions"),
        "the daemon-composed pointer for an empty task must use the no-task wording; got {:?}",
        empty_prepared.prompt
    );

    let orchestration_id = "project-launch-004-run";
    let mut started = Vec::new();
    for (role_index, role) in empty_prepared.roles.iter().enumerate() {
        let pane_id = format!("project-launch-004-{}", role.name);
        let response = daemon
            .send_attach_request(&AttachRequest::StartPreparedAgent {
                prep_token: empty_prepared.token.clone(),
                command: Some("cat".into()),
                cwd: Some(project_wire.clone()),
                rows: 24,
                cols: 80,
                env: vec![("DOT_AGENT_DECK_PANE_ID".into(), pane_id)],
                display_name: Some(role.name.clone()),
                tab_membership: Some(TabMembership::Orchestration {
                    name: "loop".into(),
                    role_index,
                    role_name: role.name.clone(),
                    is_start_role: role.start,
                    orchestration_cwd: Some(project_wire.clone()),
                    display_title: Some(RUN_TITLE.into()),
                    orchestration_id: Some(orchestration_id.into()),
                }),
                agent_type: None,
                seed: None,
            })
            .expect("StartPreparedAgent over the attach socket");
        assert!(
            response.ok,
            "starting prepared role {:?} must succeed; response error: {:?}",
            role.name, response.error
        );
        started.push(
            response
                .id
                .unwrap_or_else(|| panic!("prepared role {:?} returned no agent id", role.name)),
        );
    }

    let records = daemon.wait_for_agent_count(empty_prepared.roles.len(), Duration::from_secs(10));
    for role in &empty_prepared.roles {
        let record = records
            .iter()
            .find(|record| {
                started.contains(&record.id)
                    && record.display_name.as_deref() == Some(role.name.as_str())
            })
            .unwrap_or_else(|| {
                panic!(
                    "started role {:?} must appear in the daemon records; records: {records:?}",
                    role.name
                )
            });
        assert!(
            matches!(
                record.tab_membership.as_ref(),
                Some(TabMembership::Orchestration {
                    display_title: Some(title),
                    ..
                }) if title == RUN_TITLE
            ),
            "started role {:?} must report the user-supplied run title {:?}; membership: {:?}",
            role.name,
            RUN_TITLE,
            record.tab_membership
        );
    }

    let control_response = daemon
        .send_attach_request(&AttachRequest::PrepareWorkflow {
            path: project_wire,
            orchestration: "loop".into(),
            task: CONTROL_TASK.into(),
            config_revision: None,
        })
        .expect("non-empty control PrepareWorkflow over the attach socket");
    assert!(
        control_response.ok,
        "the non-empty control preparation must still succeed; error: {:?}",
        control_response.error
    );
    let control_prepared = control_response
        .workflow_prepared
        .expect("the non-empty control must carry a PreparedWorkflow");
    let control_context = std::fs::read_to_string(&control_prepared.context_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", control_prepared.context_path));
    for needle in ["\n## Your task\n", "\n## Task precedence\n", CONTROL_TASK] {
        assert!(
            control_context.contains(needle),
            "the non-empty control must retain the task section; missing {needle:?} from {}",
            control_prepared.context_path
        );
    }
    assert!(
        control_prepared.prompt.contains("## Your task"),
        "the non-empty control pointer must direct the coordinator to its task; got {:?}",
        control_prepared.prompt
    );
}
