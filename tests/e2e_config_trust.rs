#![cfg(all(feature = "e2e", unix))]

//! PRD #1885 — the deck runs nothing from a project's orchestration that the
//! user has not approved, or that changed since they did. L2 coverage of the
//! user-visible contract in three parts:
//!
//! - `config-trust/cli/*`: the real `dot-agent-deck trust list | approve |
//!   revoke` against an isolated state dir — no daemon involved.
//! - `config-trust/prompt/*` and `config-trust/restore/*`: the TUI's trust
//!   prompt on the New Agent form and on session restore, through a PTY.
//! - `config-trust/respawn/*`, `config-trust/schedule/001` and
//!   `config-trust/dispatch/001`: the unattended paths refuse, report the
//!   refusal to whoever asked, and start nothing; `config-trust/schedule/003`
//!   pins that a forged `.git` file borrows no approval.
//! - `config-trust/rearm/001` and `config-trust/escape/001`: a warm reattach
//!   re-arms no unapproved instructions, and refusals print no raw terminal
//!   controls from project-chosen names.
//!
//! Every role command is a token-free stand-in (`env TRUST_ROLE=<role> cat`,
//! `sleep 600`). `config-trust/schedule/002` lives in `e2e_issue_dispatch.rs`
//! beside the `gh` stub it needs, and `config-trust/live/001` in
//! `e2e_config_trust_live.rs` (lane 2).

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_view.rs"]
mod inbox_view;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use spec::spec;

/// The `config-trust` fixture's one orchestration.
const ORCH: &str = "trust-orch";
/// The fixture's `coder` command, and the same text with one character changed.
const CODER_COMMAND: &str = "env TRUST_ROLE=coder cat";
const CODER_COMMAND_EDITED: &str = "env TRUST_ROLE=coded cat";
/// The trust prompt's title and footer (the spec's binding text).
const PROMPT_TITLE: &str = "Review project commands";
const PROMPT_FOOTER: &str = "a approve and start · Esc cancel · ↑↓ scroll";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn describe(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()))
        .to_string_lossy()
        .into_owned()
}

/// The refusal every unattended path reports (spec: "Unattended refusal"):
/// the orchestration, the project, why, and the exact command that approves it.
fn assert_refusal(message: &str, project: &Path, why: &str) {
    let project = canonical(project);
    for needle in [
        ORCH,
        project.as_str(),
        why,
        "dot-agent-deck trust approve",
        &format!("--orchestration {ORCH}"),
    ] {
        assert!(
            message.contains(needle),
            "the refusal must contain {needle:?}; it read:\n{message}"
        );
    }
}

/// Write the `config-trust` orchestration into `dir` with `coder` running
/// `coder_command`, optionally with a third `reviewer` role.
fn write_trust_config(dir: &Path, coder_command: &str, with_reviewer: bool) {
    let mut toml = format!(
        "[[orchestrations]]\n\
         name = \"{ORCH}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"env TRUST_ROLE=orchestrator cat\"\n\
         start = true\n\
         description = \"TRUST-DESC-ORCH coordinates the stand-ins\"\n\
         prompt_template = \"TRUST-PROMPT-ORCH coordinate the stand-in team\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"coder\"\n\
         command = \"{coder_command}\"\n\
         clear = true\n\
         description = \"TRUST-DESC-CODER does the stand-in work\"\n\
         prompt_template = \"TRUST-PROMPT-CODER do the stand-in work\"\n"
    );
    if with_reviewer {
        toml.push_str(
            "\n[[orchestrations.roles]]\n\
             name = \"reviewer\"\n\
             command = \"env TRUST_ROLE=reviewer cat\"\n",
        );
    }
    std::fs::write(dir.join(".dot-agent-deck.toml"), toml).expect("write the trust config");
}

// ---------------------------------------------------------------------------
// config-trust/cli/*: the `trust` CLI against an isolated state dir
// ---------------------------------------------------------------------------

/// An isolated `HOME` + state dir and one git project holding the
/// `config-trust` orchestration, committed so a linked worktree can be cut.
struct CliSandbox {
    _scratch: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    state: PathBuf,
    project: PathBuf,
}

impl CliSandbox {
    fn new() -> Self {
        let scratch = common::race_safe_tempdir();
        let root = scratch.path().to_path_buf();
        let home = root.join("home");
        let state = root.join("state");
        let project = root.join("project");
        for dir in [&home, &project] {
            std::fs::create_dir_all(dir).expect("create sandbox dir");
        }
        write_trust_config(&project, CODER_COMMAND, false);
        for args in [
            &["-c", "init.defaultBranch=main", "init", "-q"][..],
            &["add", "-A"],
            &["commit", "-q", "-m", "init"],
        ] {
            let status = common::fixture_git(&project, &root)
                .args(args)
                .status()
                .expect("run git");
            assert!(
                status.success(),
                "git {args:?} failed in the sandbox project"
            );
        }
        CliSandbox {
            _scratch: scratch,
            root,
            home,
            state,
            project,
        }
    }

    /// Run `dot-agent-deck trust <args…>` from `cwd` with no terminal, the
    /// sandbox's HOME and state dir, and `extra` on top.
    fn trust_from(&self, cwd: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut cmd = std::process::Command::new(common::contained_deck_binary());
        cmd.arg("trust")
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("HOME", &self.home)
            .env("DOT_AGENT_DECK_STATE_DIR", &self.state)
            .stdin(std::process::Stdio::null());
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().expect("run `dot-agent-deck trust`")
    }

    fn trust(&self, args: &[&str]) -> Output {
        self.trust_from(&self.project, args, &[])
    }

    fn project_arg(&self) -> String {
        self.project.to_string_lossy().into_owned()
    }

    /// `trust list`'s stdout, asserting it exits 0.
    fn list(&self) -> String {
        let listed = self.trust(&["list"]);
        assert!(
            listed.status.success(),
            "`trust list` must exit 0: {}",
            describe(&listed)
        );
        String::from_utf8_lossy(&listed.stdout).into_owned()
    }

    /// The `list` line for this project's orchestration, if any.
    fn list_line(&self) -> Option<String> {
        let project = canonical(&self.project);
        self.list()
            .lines()
            .find(|l| l.contains(&project) && l.contains(ORCH))
            .map(str::to_string)
    }

    fn approve_yes(&self) {
        let approved = self.trust(&["approve", &self.project_arg(), "--yes"]);
        assert!(
            approved.status.success(),
            "`trust approve <dir> --yes` must approve: {}",
            describe(&approved)
        );
    }
}

fn state_word(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find(|w| matches!(*w, "current" | "changed" | "missing"))
}

/// Scenario: In an isolated state dir, run `trust approve <project> --yes` on
/// a project whose orchestration has two roles. It must exit 0 and print each
/// role's command, description and prompt text, and `trust list` must then
/// show one line for the project and orchestration marked `current`.
#[spec("config-trust/cli/001")]
#[test]
fn cli_001_approve_yes_then_list_shows_current() {
    let sandbox = CliSandbox::new();
    let empty = sandbox.list();
    assert!(
        !empty.trim().is_empty() && !empty.contains(ORCH),
        "with no approvals `trust list` prints a line saying so, naming nothing: {empty:?}"
    );

    let approved = sandbox.trust(&["approve", &sandbox.project_arg(), "--yes"]);
    assert!(
        approved.status.success(),
        "approve --yes: {}",
        describe(&approved)
    );
    let printed = String::from_utf8_lossy(&approved.stdout);
    for needle in [
        ORCH,
        "orchestrator",
        "coder",
        "env TRUST_ROLE=orchestrator cat",
        CODER_COMMAND,
        "TRUST-DESC-ORCH coordinates the stand-ins",
        "TRUST-PROMPT-CODER do the stand-in work",
    ] {
        assert!(
            printed.contains(needle),
            "`--yes` still prints what it approved; {needle:?} is missing from:\n{printed}"
        );
    }

    let line = sandbox
        .list_line()
        .unwrap_or_else(|| panic!("`trust list` must list the approval:\n{}", sandbox.list()));
    assert_eq!(state_word(&line), Some("current"), "list line: {line:?}");
}

/// Scenario: Approve a project with `--yes`, then change one character of a
/// role's `command` in its `.dot-agent-deck.toml`. `trust list` must now mark
/// the approval `changed`, and putting the text back marks it `current` again.
#[spec("config-trust/cli/002")]
#[test]
fn cli_002_editing_one_character_reads_as_changed() {
    let sandbox = CliSandbox::new();
    sandbox.approve_yes();
    write_trust_config(&sandbox.project, CODER_COMMAND_EDITED, false);
    let line = sandbox.list_line().expect("the approval is still listed");
    assert_eq!(state_word(&line), Some("changed"), "list line: {line:?}");

    write_trust_config(&sandbox.project, CODER_COMMAND, false);
    let line = sandbox.list_line().expect("the approval is still listed");
    assert_eq!(state_word(&line), Some("current"), "list line: {line:?}");
}

/// Scenario: Approve a project, then `trust revoke <project>`: it exits 0 and
/// `trust list` no longer shows the project. A second revoke has nothing to
/// remove, so it exits non-zero with a message.
#[spec("config-trust/cli/003")]
#[test]
fn cli_003_revoke_removes_and_a_second_revoke_fails() {
    let sandbox = CliSandbox::new();
    sandbox.approve_yes();
    let revoked = sandbox.trust(&["revoke", &sandbox.project_arg()]);
    assert!(revoked.status.success(), "revoke: {}", describe(&revoked));
    assert!(
        sandbox.list_line().is_none(),
        "a revoked approval must not be listed:\n{}",
        sandbox.list()
    );

    let again = sandbox.trust(&["revoke", &sandbox.project_arg()]);
    assert!(
        !again.status.success() && !text(&again).trim().is_empty(),
        "revoking when nothing is approved must fail with a message: {}",
        describe(&again)
    );
}

/// Scenario: Run `trust approve <project>` with no terminal on stdin and no
/// `--yes`. It must refuse — non-zero exit, a message naming `--yes` — and
/// store nothing, so `trust list` still shows no approval for the project.
#[spec("config-trust/cli/004")]
#[test]
fn cli_004_approve_without_a_terminal_or_yes_refuses() {
    let sandbox = CliSandbox::new();
    let refused = sandbox.trust(&["approve", &sandbox.project_arg()]);
    assert!(
        !refused.status.success(),
        "approve with no TTY and no --yes must refuse: {}",
        describe(&refused)
    );
    assert!(
        text(&refused).contains("--yes"),
        "the refusal must name --yes: {}",
        describe(&refused)
    );
    assert!(
        sandbox.list_line().is_none(),
        "a refused approve must store nothing:\n{}",
        sandbox.list()
    );
}

/// Scenario: Run `trust approve <project> --yes` with the deck's pane identity
/// (`DOT_AGENT_DECK_PANE_ID`) in its environment, as from inside a pane the
/// deck started. It must refuse with `run it from a terminal outside the deck`
/// and store nothing, while `trust list` and `trust revoke` still run there.
#[spec("config-trust/cli/005")]
#[test]
fn cli_005_approve_inside_a_deck_pane_refuses() {
    let sandbox = CliSandbox::new();
    let pane = [(dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID, "pane-7")];
    let refused = sandbox.trust_from(
        &sandbox.project,
        &["approve", &sandbox.project_arg(), "--yes"],
        &pane,
    );
    assert!(
        !refused.status.success(),
        "approve inside a deck pane must refuse even with --yes: {}",
        describe(&refused)
    );
    assert!(
        text(&refused).contains("run it from a terminal outside the deck"),
        "the refusal must say where to run it: {}",
        describe(&refused)
    );
    assert!(
        sandbox.list_line().is_none(),
        "nothing was approved:\n{}",
        sandbox.list()
    );

    let listed = sandbox.trust_from(&sandbox.project, &["list"], &pane);
    assert!(
        listed.status.success(),
        "list is allowed in a pane: {}",
        describe(&listed)
    );
    sandbox.approve_yes();
    let revoked = sandbox.trust_from(&sandbox.project, &["revoke", &sandbox.project_arg()], &pane);
    assert!(
        revoked.status.success(),
        "revoke is allowed in a pane: {}",
        describe(&revoked)
    );
    assert!(
        sandbox.list_line().is_none(),
        "revoked:\n{}",
        sandbox.list()
    );
}

/// Scenario: Cut a linked git worktree of the project and run `trust approve
/// <worktree> --yes`. The approval must be stored under the MAIN checkout (the
/// list line names the main path, not the worktree's), and `trust revoke
/// <main checkout>` must then find and remove it.
#[spec("config-trust/cli/006")]
#[test]
fn cli_006_a_linked_worktree_shares_its_main_checkouts_approval() {
    let sandbox = CliSandbox::new();
    let worktree = sandbox.root.join("project-wt");
    let added = common::fixture_git(&sandbox.project, &sandbox.root)
        .args(["worktree", "add", "-q", "-b", "wt"])
        .arg(&worktree)
        .status()
        .expect("git worktree add");
    assert!(added.success(), "git worktree add failed");

    let worktree_arg = worktree.to_string_lossy().into_owned();
    let approved = sandbox.trust_from(&worktree, &["approve", &worktree_arg, "--yes"], &[]);
    assert!(
        approved.status.success(),
        "approve from the worktree: {}",
        describe(&approved)
    );

    let listed = sandbox.list();
    let worktree_canonical = canonical(&worktree);
    let line = sandbox.list_line().unwrap_or_else(|| {
        panic!("the approval must be listed under the main checkout:\n{listed}")
    });
    assert_eq!(state_word(&line), Some("current"), "list line: {line:?}");
    assert!(
        !listed.contains(&worktree_canonical),
        "a linked worktree is not a project of its own; the list names it:\n{listed}"
    );

    let revoked = sandbox.trust(&["revoke", &sandbox.project_arg()]);
    assert!(
        revoked.status.success(),
        "the main checkout must find the approval its worktree made: {}",
        describe(&revoked)
    );
}

// ---------------------------------------------------------------------------
// TUI helpers
// ---------------------------------------------------------------------------

/// Ctrl+n, confirm the current directory, choose the fixture's (single)
/// orchestration and submit — the `open_orchestration` sequence the
/// `pane/spawn` and `pane/restart` tests use.
fn choose_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // confirm current dir -> New Agent form
    deck.wait_for_string("No mode");
    deck.send_keys(b"\x1b[C"); // Right -> [Orch: trust-orch]
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // submit (Command is hidden for an orchestration)
}

fn role_records(attach: &Path) -> Vec<AgentRecord> {
    common::agent_records_on(attach)
        .into_iter()
        .filter(|r| matches!(r.tab_membership, Some(TabMembership::Orchestration { .. })))
        .collect()
}

fn role_record(attach: &Path, role: &str) -> Option<AgentRecord> {
    role_records(attach).into_iter().find(|r| {
        matches!(
            &r.tab_membership,
            Some(TabMembership::Orchestration { role_name, .. }) if role_name == role
        )
    })
}

/// Whether no orchestration role appears on the daemon for the whole `window`.
fn no_roles_for(attach: &Path, window: Duration) -> bool {
    !common::wait_until(window, || !role_records(attach).is_empty())
}

fn wait_for_prompt(deck: &TuiDeck) -> String {
    let shown = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        deck.snapshot_grid().contains(PROMPT_TITLE)
    });
    assert!(
        shown,
        "the trust prompt ({PROMPT_TITLE:?}) never appeared.\nRoles on the daemon: {:?}\nGrid:\n{}",
        role_records(deck.attach_socket_path())
            .iter()
            .map(|r| &r.tab_membership)
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
    deck.snapshot_grid()
}

/// Whether some line of `grid` shows `role` marked `changed`.
fn role_marked_changed(grid: &str, role: &str) -> bool {
    grid.lines()
        .any(|l| l.contains(role) && l.contains("changed"))
}

// ---------------------------------------------------------------------------
// config-trust/prompt/*: the New Agent form's trust prompt
// ---------------------------------------------------------------------------

/// Scenario: Launch the deck on a project whose orchestration was never
/// approved, press Ctrl+n and choose it. The `Review project commands` prompt
/// must show the project, the orchestration, `not approved before`, each
/// role's command, description and prompt text and the footer, with nothing
/// spawned. `Esc` closes it with nothing spawned; choosing again and pressing
/// `a` starts both roles and `trust list` shows the approval `current`.
#[spec("config-trust/prompt/001")]
#[test]
fn prompt_001_an_unapproved_orchestration_asks_before_anything_starts() {
    let deck = TuiDeck::builder()
        .without_trust_preapproval()
        .with_pty_size(140, 50)
        .launch_with_fixture("config-trust");
    deck.wait_for_string("No active agents");
    let attach = deck.attach_socket_path().to_path_buf();

    choose_orchestration(&deck);
    let grid = wait_for_prompt(&deck);
    let project_name = Path::new(&canonical(deck.workdir()))
        .file_name()
        .expect("the project dir has a name")
        .to_string_lossy()
        .into_owned();
    for needle in [
        project_name.as_str(),
        ORCH,
        "not approved before",
        "orchestrator",
        "env TRUST_ROLE=orchestrator cat",
        CODER_COMMAND,
        "TRUST-DESC-ORCH",
        "TRUST-PROMPT-ORCH",
        "TRUST-DESC-CODER",
        "TRUST-PROMPT-CODER",
        PROMPT_FOOTER,
    ] {
        assert!(
            grid.contains(needle),
            "the trust prompt must show {needle:?}.\nGrid:\n{grid}"
        );
    }
    assert!(
        no_roles_for(&attach, Duration::from_secs(2)),
        "nothing may be spawned while the prompt is open: {:?}",
        role_records(&attach)
    );

    deck.send_keys(b"\x1b"); // Esc -> cancel
    deck.wait_for_absence(PROMPT_TITLE);
    assert!(
        no_roles_for(&attach, Duration::from_secs(2)),
        "Esc must spawn nothing: {:?}",
        role_records(&attach)
    );

    choose_orchestration(&deck);
    wait_for_prompt(&deck);
    deck.send_keys(b"a"); // approve and start
    let started = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        role_record(&attach, "orchestrator").is_some() && role_record(&attach, "coder").is_some()
    });
    assert!(
        started,
        "`a` must approve and start both roles; roles = {:?}\nGrid:\n{}",
        role_records(&attach)
            .iter()
            .map(|r| &r.tab_membership)
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );

    let listed = deck.run_deck_cli(deck.workdir(), &["trust", "list"]);
    let listed = text(&listed);
    assert!(
        listed
            .lines()
            .any(|l| l.contains(ORCH) && state_word(l) == Some("current")),
        "approving in the prompt must store the approval:\n{listed}"
    );
}

/// Scenario: Launch on an approved project, then change one character of the
/// `coder` role's command on disk. Ctrl+n and choosing the orchestration must
/// show the prompt with `coder` marked `changed`. Pressing Enter must not
/// approve: the prompt stays and nothing is spawned.
#[spec("config-trust/prompt/002")]
#[test]
fn prompt_002_a_changed_role_is_marked_and_enter_does_not_approve() {
    let deck = TuiDeck::builder()
        .with_pty_size(140, 50)
        .launch_with_fixture("config-trust");
    deck.wait_for_string("No active agents");
    let attach = deck.attach_socket_path().to_path_buf();
    write_trust_config(deck.workdir(), CODER_COMMAND_EDITED, false);

    choose_orchestration(&deck);
    let grid = wait_for_prompt(&deck);
    assert!(
        role_marked_changed(&grid, "coder"),
        "the role whose command changed must be marked `changed`.\nGrid:\n{grid}"
    );
    assert!(
        grid.contains(CODER_COMMAND_EDITED),
        "the prompt shows the command as it reads now.\nGrid:\n{grid}"
    );

    deck.send_keys(b"\r"); // Enter must NOT approve
    assert!(
        no_roles_for(&attach, Duration::from_secs(3)),
        "Enter must not approve or spawn anything: {:?}",
        role_records(&attach)
    );
    assert!(
        deck.snapshot_grid().contains(PROMPT_TITLE),
        "the prompt stays open after Enter.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    let listed = text(&deck.run_deck_cli(deck.workdir(), &["trust", "list"]));
    assert!(
        listed
            .lines()
            .any(|l| l.contains(ORCH) && state_word(l) == Some("changed")),
        "the stored approval is still the old text:\n{listed}"
    );
}

// ---------------------------------------------------------------------------
// config-trust/restore/001: session restore
// ---------------------------------------------------------------------------

/// TOML basic-string escape for the paths and names staged below.
fn toml_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Stage a `session.toml` holding one orchestration pane, in the
/// `OrchestrationSnapshot` key names `session/restore/008` stages.
fn stage_orchestration_session(session_file: &Path, project: &Path, roles: &[&str]) {
    stage_orchestration_session_running(session_file, project, roles, "sleep 600");
}

/// [`stage_orchestration_session`] with the saved orchestrator pane's
/// `command` set to `command` — what a plain-pane restore of it would run.
fn stage_orchestration_session_running(
    session_file: &Path,
    project: &Path,
    roles: &[&str],
    command: &str,
) {
    let command = toml_str(command);
    let dir = toml_str(&project.to_string_lossy());
    let roles = roles
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!(
        "[[panes]]\n\
         dir = \"{dir}\"\n\
         name = \"orchestrator\"\n\
         command = \"{command}\"\n\n\
         [panes.orchestration]\n\
         version = 1\n\
         roles = [{roles}]\n\
         start_role_index = 0\n\
         orchestrator_prompt = \"TRUST-RESTORED-PROMPT\"\n\
         config_name = \"restore-orch\"\n\
         project_path = \"{dir}\"\n\
         started_role_indices = [0, 1, 2]\n"
    );
    std::fs::write(session_file, toml).expect("stage session.toml");
}

fn write_restore_config(project: &Path, coder_command: &str) {
    let toml = format!(
        "[[orchestrations]]\n\
         name = \"restore-orch\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"sleep 600\"\n\
         start = true\n\n\
         [[orchestrations.roles]]\n\
         name = \"coder\"\n\
         command = \"{coder_command}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"reviewer\"\n\
         command = \"sleep 600\"\n"
    );
    std::fs::write(project.join(".dot-agent-deck.toml"), toml).expect("write restore config");
}

/// Run `dot-agent-deck trust <args…>` from `project` with no terminal, in
/// `home` and the `state` dir the restore tests hand the deck.
fn restore_trust_cli(project: &Path, state: &Path, home: &Path, args: &[&str]) -> Output {
    let mut cmd = std::process::Command::new(common::contained_deck_binary());
    cmd.arg("trust")
        .args(args)
        .current_dir(project)
        .env_clear()
        .env("HOME", home)
        .env("DOT_AGENT_DECK_STATE_DIR", state)
        .stdin(std::process::Stdio::null());
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.output().expect("run `dot-agent-deck trust`")
}

/// Scenario: Approve a project's orchestration with the real CLI, change one
/// character of its `coder` command, and stage a saved session holding that
/// orchestration. Launching the TUI on an empty daemon must show the trust
/// prompt with `coder` marked `changed` and the action `a approve and
/// restore`, with none of its roles spawned; pressing `a` restores them.
#[spec("config-trust/restore/001")]
#[test]
fn restore_001_a_changed_restored_orchestration_asks_before_restoring() {
    let scratch = common::race_safe_tempdir();
    let project = scratch.path().join("restore-project");
    let state = scratch.path().join("state");
    let home = scratch.path().join("approve-home");
    std::fs::create_dir_all(&project).expect("create project");
    std::fs::create_dir_all(&home).expect("create approve HOME");
    write_restore_config(&project, "sleep 600");

    // Approve exactly as a user does, into the state dir the deck will use.
    let approved = restore_trust_cli(
        &project,
        &state,
        &home,
        &["approve", &project.to_string_lossy(), "--yes"],
    );
    assert!(
        approved.status.success(),
        "precondition approve: {}",
        describe(&approved)
    );

    write_restore_config(&project, "sleep 601"); // one character changed
    let session_file = scratch.path().join("session.toml");
    stage_orchestration_session(
        &session_file,
        &project,
        &["orchestrator", "coder", "reviewer"],
    );

    let deck = TuiDeck::builder()
        .with_pty_size(140, 50)
        .with_env("DOT_AGENT_DECK_SESSION", session_file.to_string_lossy())
        .with_env("DOT_AGENT_DECK_STATE_DIR", state.to_string_lossy())
        .launch_with_fixture("plain-project");
    let attach = deck.attach_socket_path().to_path_buf();

    let grid = wait_for_prompt(&deck);
    for needle in ["restore-orch", "a approve and restore", "sleep 601"] {
        assert!(
            grid.contains(needle),
            "the restore prompt must show {needle:?}.\nGrid:\n{grid}"
        );
    }
    assert!(
        role_marked_changed(&grid, "coder"),
        "the changed role must be marked `changed`.\nGrid:\n{grid}"
    );
    assert!(
        no_roles_for(&attach, Duration::from_secs(2)),
        "a changed orchestration restores nothing until approved: {:?}",
        role_records(&attach)
    );

    deck.send_keys(b"a");
    let restored = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        role_record(&attach, "coder").is_some() && role_record(&attach, "reviewer").is_some()
    });
    assert!(
        restored,
        "`a` must approve and restore the orchestration; roles = {:?}\nGrid:\n{}",
        role_records(&attach)
            .iter()
            .map(|r| &r.tab_membership)
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
}

/// Scenario: Approve a project's orchestration with the real CLI, change one
/// character of its `coder` command, and stage a saved session holding that
/// orchestration followed by a plain `sleep 600` pane. Launching the TUI shows
/// the restore prompt; `Esc` closes it, none of the orchestration's roles
/// start, the plain pane is restored, and `trust list` still reads `changed`.
#[spec("config-trust/restore/002")]
#[test]
fn restore_002_cancelling_the_restore_prompt_leaves_the_orchestration_unrestored() {
    const PLAIN: &str = "r-plain";
    let scratch = common::race_safe_tempdir();
    let project = scratch.path().join("restore-project");
    let plain_dir = scratch.path().join("plain-dir");
    let state = scratch.path().join("state");
    let home = scratch.path().join("approve-home");
    for dir in [&project, &plain_dir, &home] {
        std::fs::create_dir_all(dir).expect("create sandbox dir");
    }
    write_restore_config(&project, "sleep 600");
    let approved = restore_trust_cli(
        &project,
        &state,
        &home,
        &["approve", &project.to_string_lossy(), "--yes"],
    );
    assert!(
        approved.status.success(),
        "precondition approve: {}",
        describe(&approved)
    );
    write_restore_config(&project, "sleep 601"); // one character changed

    // The orchestration pane first, so a restore that stops at the cancelled
    // orchestration instead of carrying on would also lose the plain pane.
    let session_file = scratch.path().join("session.toml");
    stage_orchestration_session(
        &session_file,
        &project,
        &["orchestrator", "coder", "reviewer"],
    );
    let mut session = std::fs::read_to_string(&session_file).expect("read staged session");
    session.push_str(&format!(
        "\n[[panes]]\ndir = \"{}\"\nname = \"{PLAIN}\"\ncommand = \"sleep 600\"\n",
        toml_str(&plain_dir.to_string_lossy())
    ));
    std::fs::write(&session_file, session).expect("stage the plain pane");

    let deck = TuiDeck::builder()
        .with_pty_size(140, 50)
        .with_env("DOT_AGENT_DECK_SESSION", session_file.to_string_lossy())
        .with_env("DOT_AGENT_DECK_STATE_DIR", state.to_string_lossy())
        .launch_with_fixture("plain-project");
    let attach = deck.attach_socket_path().to_path_buf();

    let grid = wait_for_prompt(&deck);
    assert!(
        grid.contains("a approve and restore") && grid.contains("restore-orch"),
        "precondition: the restore prompt for `restore-orch`.\nGrid:\n{grid}"
    );

    deck.send_keys(b"\x1b"); // Esc
    let closed = common::wait_until(common::load_scaled(Duration::from_secs(5)), || {
        !deck.snapshot_grid().contains(PROMPT_TITLE)
    });
    assert!(
        closed,
        "`Esc` must close the restore prompt.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    let plain_restored = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        deck.snapshot_grid().contains(PLAIN)
            && common::agent_records_on(&attach)
                .iter()
                .any(|r| r.display_name.as_deref() == Some(PLAIN))
    });
    assert!(
        plain_restored,
        "cancelling one orchestration must still restore the rest of the session (`{PLAIN}`).\nAgents: {:?}\nGrid:\n{}",
        common::agent_records_on(&attach)
            .iter()
            .map(|r| &r.display_name)
            .collect::<Vec<_>>(),
        deck.snapshot_grid()
    );
    assert!(
        no_roles_for(&attach, Duration::from_secs(3)),
        "a cancelled restore must start none of the orchestration's roles: {:?}",
        role_records(&attach)
            .iter()
            .map(|r| &r.tab_membership)
            .collect::<Vec<_>>()
    );

    let listed = restore_trust_cli(&project, &state, &home, &["list"]);
    let project_canonical = canonical(&project);
    let line = text(&listed)
        .lines()
        .find(|l| l.contains(&project_canonical) && l.contains("restore-orch"))
        .map(str::to_string);
    assert_eq!(
        line.as_deref().and_then(state_word),
        Some("changed"),
        "`Esc` must store nothing: the approval still reads `changed`.\n{}",
        describe(&listed)
    );
}

/// How the project config has drifted from a saved orchestration snapshot
/// before the TUI restores it (`config-trust/restore/003`).
enum RestoreDrift {
    /// A worker role was removed from the orchestration.
    WorkerRemoved,
    /// The orchestration was renamed.
    Renamed,
    /// The project's `.dot-agent-deck.toml` was deleted.
    ConfigDeleted,
}

/// Stage a saved session holding a never-approved orchestration whose saved
/// orchestrator command writes a sentinel file, followed by a plain pane; then
/// drift the project config by `drift` and launch the TUI on an empty daemon.
/// Asserts the saved command never runs, nothing of the orchestration is
/// restored as a plain pane, and the plain pane still restores.
fn assert_drifted_restore_runs_nothing(drift: RestoreDrift) {
    const PLAIN: &str = "r-plain";
    let scratch = common::race_safe_tempdir();
    let project = scratch.path().join("restore-project");
    let plain_dir = scratch.path().join("plain-dir");
    let state = scratch.path().join("state");
    for dir in [&project, &plain_dir] {
        std::fs::create_dir_all(dir).expect("create sandbox dir");
    }
    let sentinel = scratch.path().join("restore-drift-sentinel");
    let sentinel_command = format!(
        "sh -c 'touch {}; exec sleep 600'",
        sentinel.to_string_lossy()
    );
    let role = |name: &str, command: &str, start: bool| {
        format!(
            "\n[[orchestrations.roles]]\nname = \"{name}\"\ncommand = \"{}\"\n{}",
            toml_str(command),
            if start { "start = true\n" } else { "" }
        )
    };
    let config = |orchestration: &str, with_reviewer: bool| {
        let mut toml = format!("[[orchestrations]]\nname = \"{orchestration}\"\n");
        toml.push_str(&role("orchestrator", &sentinel_command, true));
        toml.push_str(&role("coder", "sleep 600", false));
        if with_reviewer {
            toml.push_str(&role("reviewer", "sleep 600", false));
        }
        toml
    };
    let config_path = project.join(".dot-agent-deck.toml");
    match drift {
        RestoreDrift::WorkerRemoved => std::fs::write(&config_path, config("restore-orch", false)),
        RestoreDrift::Renamed => std::fs::write(&config_path, config("renamed-orch", true)),
        RestoreDrift::ConfigDeleted => Ok(()),
    }
    .expect("write the drifted config");

    let session_file = scratch.path().join("session.toml");
    stage_orchestration_session_running(
        &session_file,
        &project,
        &["orchestrator", "coder", "reviewer"],
        &sentinel_command,
    );
    let mut session = std::fs::read_to_string(&session_file).expect("read staged session");
    session.push_str(&format!(
        "\n[[panes]]\ndir = \"{}\"\nname = \"{PLAIN}\"\ncommand = \"sleep 600\"\n",
        toml_str(&plain_dir.to_string_lossy())
    ));
    std::fs::write(&session_file, session).expect("stage the plain pane");

    let deck = TuiDeck::builder()
        .with_pty_size(140, 50)
        .with_env("DOT_AGENT_DECK_SESSION", session_file.to_string_lossy())
        .with_env("DOT_AGENT_DECK_STATE_DIR", state.to_string_lossy())
        .launch_with_fixture("plain-project");
    let attach = deck.attach_socket_path().to_path_buf();

    // A trust prompt is an acceptable answer to a snapshot that cannot be
    // checked; cancel it so the rest of the session restores either way.
    let plain_restored = common::wait_until(common::load_scaled(Duration::from_secs(20)), || {
        if deck.snapshot_grid().contains(PROMPT_TITLE) {
            deck.send_keys(b"\x1b");
        }
        common::agent_records_on(&attach)
            .iter()
            .any(|r| r.display_name.as_deref() == Some(PLAIN))
    });
    // Give a saved command that did start time to write its sentinel.
    let sentinel_written = common::wait_until(Duration::from_secs(3), || sentinel.exists());
    let agents = common::agent_records_on(&attach)
        .iter()
        .map(|r| (r.display_name.clone(), r.cwd.clone()))
        .collect::<Vec<_>>();
    assert!(
        !sentinel_written,
        "the saved orchestrator command of an orchestration that cannot be trust-checked ran \
         (sentinel {} written) — a drifted, unapproved snapshot must not fall through to a \
         plain-pane restore.\nAgents: {agents:?}\nGrid:\n{}",
        sentinel.display(),
        deck.snapshot_grid()
    );
    let project_canonical = canonical(&project);
    assert!(
        !common::agent_records_on(&attach).iter().any(|r| {
            r.display_name.as_deref() == Some("orchestrator")
                || r.cwd.as_deref().is_some_and(|cwd| {
                    std::fs::canonicalize(cwd)
                        .is_ok_and(|c| c.to_string_lossy() == project_canonical)
                })
        }),
        "nothing of the orchestration may be restored as a plain pane.\nAgents: {agents:?}"
    );
    assert!(
        no_roles_for(&attach, Duration::from_secs(1)),
        "no orchestration role starts: {:?}",
        role_records(&attach)
            .iter()
            .map(|r| &r.tab_membership)
            .collect::<Vec<_>>()
    );
    assert!(
        plain_restored,
        "the rest of the session (`{PLAIN}`) must still restore.\nAgents: {agents:?}\nGrid:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Stage a saved session holding a never-approved orchestration (its
/// saved orchestrator command writes a sentinel file) and a plain pane, then
/// remove a worker role from the project config. Launching the TUI must not run
/// the saved command or restore it as a plain pane, and the plain pane restores.
#[spec("config-trust/restore/003")]
#[test]
fn restore_003_a_removed_worker_does_not_fall_through_to_a_plain_restore() {
    assert_drifted_restore_runs_nothing(RestoreDrift::WorkerRemoved);
}

/// Scenario: Stage the same saved session, then rename the orchestration in the
/// project config. Launching the TUI must not run the saved orchestrator
/// command (no sentinel file) or restore it as a plain pane, and the plain pane
/// restores.
#[spec("config-trust/restore/003")]
#[test]
fn restore_003_a_renamed_orchestration_does_not_fall_through_to_a_plain_restore() {
    assert_drifted_restore_runs_nothing(RestoreDrift::Renamed);
}

/// Scenario: Stage the same saved session, then delete the project's
/// `.dot-agent-deck.toml`. Launching the TUI must not run the saved
/// orchestrator command (no sentinel file) or restore it as a plain pane, and
/// the plain pane restores.
#[spec("config-trust/restore/003")]
#[test]
fn restore_003_a_deleted_config_does_not_fall_through_to_a_plain_restore() {
    assert_drifted_restore_runs_nothing(RestoreDrift::ConfigDeleted);
}

// ---------------------------------------------------------------------------
// config-trust/respawn/*: agent verbs refuse after a config change
// ---------------------------------------------------------------------------

/// A deck on the (pre-approved) `config-trust` fixture with its orchestration
/// open and both roles running. Returns the deck, the orchestrator's pane id
/// and coder's current agent id.
fn open_approved_orchestration() -> (TuiDeck, String, String) {
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(140, 50)
        .launch_with_fixture("config-trust");
    deck.wait_for_string("No active agents");
    choose_orchestration(&deck);
    let attach = deck.attach_socket_path().to_path_buf();
    let up = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        role_record(&attach, "orchestrator").is_some() && role_record(&attach, "coder").is_some()
    });
    assert!(
        up,
        "precondition: the approved orchestration opens with both roles, no prompt.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    let orchestrator = role_record(&attach, "orchestrator")
        .and_then(|r| r.pane_id_env)
        .expect("the orchestrator has a pane id");
    let coder = role_record(&attach, "coder").expect("coder is up").id;
    (deck, orchestrator, coder)
}

/// Run `dot-agent-deck <args…>` as the agent in `caller_pane` would.
fn agent_verb(deck: &TuiDeck, caller_pane: &str, args: &[&str]) -> Output {
    std::process::Command::new(common::contained_deck_binary())
        .args(args)
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("DOT_AGENT_DECK_PANE_ID", caller_pane)
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run the agent verb")
}

/// Coder's agent id must still be the one it had before the refused verb.
fn assert_coder_untouched(deck: &TuiDeck, coder_before: &str) {
    let attach = deck.attach_socket_path();
    let unchanged = !common::wait_until(Duration::from_secs(2), || {
        role_record(attach, "coder").is_none_or(|r| r.id != coder_before)
    });
    assert!(
        unchanged,
        "a refused verb must start no new process for coder (was {coder_before}); roles = {:?}",
        role_records(attach)
            .iter()
            .map(|r| (&r.id, &r.tab_membership))
            .collect::<Vec<_>>()
    );
}

/// Scenario: Open the approved orchestration, then change one character of the
/// `clear = true` coder role's command on disk. `delegate --to coder` from the
/// orchestrator must fail with the refusal (orchestration, project, `changed
/// since you approved it`, the approve command) and coder keeps its process.
#[spec("config-trust/respawn/001")]
#[test]
fn respawn_001_delegate_to_a_clear_role_refuses_after_a_change() {
    let (deck, orchestrator, coder) = open_approved_orchestration();
    write_trust_config(deck.workdir(), CODER_COMMAND_EDITED, false);

    let out = agent_verb(
        &deck,
        &orchestrator,
        &["delegate", "--to", "coder", "--task", "TRUST-DELEGATE-TASK"],
    );
    assert!(
        !out.status.success(),
        "the delegate must be refused: {}",
        describe(&out)
    );
    assert_refusal(&text(&out), deck.workdir(), "changed since you approved it");
    assert_coder_untouched(&deck, &coder);
}

/// Scenario: Open the approved orchestration, change one character of coder's
/// command, then run `pane restart coder --force` from the orchestrator. It must
/// fail with the refusal text and coder keeps its process.
#[spec("config-trust/respawn/002")]
#[test]
fn respawn_002_pane_restart_refuses_after_a_change() {
    let (deck, orchestrator, coder) = open_approved_orchestration();
    write_trust_config(deck.workdir(), CODER_COMMAND_EDITED, false);

    let out = agent_verb(
        &deck,
        &orchestrator,
        &["pane", "restart", "coder", "--force"],
    );
    assert!(
        !out.status.success(),
        "the restart must be refused: {}",
        describe(&out)
    );
    assert_refusal(&text(&out), deck.workdir(), "changed since you approved it");
    assert_coder_untouched(&deck, &coder);
}

/// Scenario: Open the approved orchestration, add a `reviewer` role to its
/// config on disk, then run `pane spawn reviewer` from the orchestrator. It
/// must fail with the refusal text, and no reviewer appears on the daemon.
#[spec("config-trust/respawn/003")]
#[test]
fn respawn_003_pane_spawn_refuses_after_a_change() {
    let (deck, orchestrator, _coder) = open_approved_orchestration();
    write_trust_config(deck.workdir(), CODER_COMMAND, true);

    let out = agent_verb(&deck, &orchestrator, &["pane", "spawn", "reviewer"]);
    assert!(
        !out.status.success(),
        "the spawn must be refused: {}",
        describe(&out)
    );
    assert_refusal(&text(&out), deck.workdir(), "changed since you approved it");
    let attach = deck.attach_socket_path();
    assert!(
        !common::wait_until(Duration::from_secs(2), || role_record(attach, "reviewer")
            .is_some()),
        "a refused spawn starts no reviewer"
    );
}

/// The `config-trust` orchestration plus a non-clear `reviewer` role whose
/// `prompt_template` is `reviewer_prompt`.
fn write_trust_config_with_reviewer_prompt(dir: &Path, reviewer_prompt: &str) {
    write_trust_config(dir, CODER_COMMAND, false);
    let path = dir.join(".dot-agent-deck.toml");
    let mut toml = std::fs::read_to_string(&path).expect("read the trust config");
    toml.push_str(&format!(
        "\n[[orchestrations.roles]]\n\
         name = \"reviewer\"\n\
         command = \"env TRUST_ROLE=reviewer cat\"\n\
         description = \"TRUST-DESC-REVIEWER reviews the stand-in work\"\n\
         prompt_template = \"{reviewer_prompt}\"\n"
    ));
    std::fs::write(&path, toml).expect("write the trust config");
}

/// Scenario: Approve and open an orchestration with a non-clear `reviewer`
/// role, then change only that role's `prompt_template` on disk. `delegate --to
/// reviewer` from the orchestrator must fail with the refusal text, and the task
/// never reaches the reviewer's inbox.
#[spec("config-trust/respawn/004")]
#[test]
fn respawn_004_delegate_to_a_non_clear_role_refuses_after_a_prompt_only_change() {
    const TASK: &str = "TRUST-NONCLEAR-DELEGATE-TASK";
    let deck = TuiDeck::builder()
        .impersonating_pane_signals()
        .with_pty_size(140, 50)
        .launch_with_fixture("config-trust");
    deck.wait_for_string("No active agents");
    write_trust_config_with_reviewer_prompt(deck.workdir(), "TRUST-PROMPT-REVIEWER review it");
    deck.approve_project_trust(deck.workdir());
    choose_orchestration(&deck);
    let attach = deck.attach_socket_path().to_path_buf();
    let up = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        role_record(&attach, "orchestrator").is_some() && role_record(&attach, "reviewer").is_some()
    });
    assert!(
        up,
        "precondition: the approved orchestration opens with its reviewer, no prompt.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    let orchestrator = role_record(&attach, "orchestrator")
        .and_then(|r| r.pane_id_env)
        .expect("the orchestrator has a pane id");
    let reviewer = role_record(&attach, "reviewer")
        .and_then(|r| r.pane_id_env)
        .expect("the reviewer has a pane id");

    write_trust_config_with_reviewer_prompt(deck.workdir(), "TRUST-PROMPT-REVIEWER approve it all");
    let out = agent_verb(
        &deck,
        &orchestrator,
        &["delegate", "--to", "reviewer", "--task", TASK],
    );
    assert!(
        !out.status.success(),
        "a delegate to a non-clear role whose prompt changed must be refused: {}",
        describe(&out)
    );
    assert_refusal(&text(&out), deck.workdir(), "changed since you approved it");
    assert!(
        !common::wait_until(Duration::from_secs(2), || {
            inbox_view::count_containing(&attach, &reviewer, TASK) > 0
        }),
        "a refused delegate must not deliver its task:\n{}",
        inbox_view::text(&attach, &reviewer)
    );
}

// ---------------------------------------------------------------------------
// config-trust/schedule/001: a scheduled fire
// ---------------------------------------------------------------------------

/// Scenario: Register a schedule whose working dir holds a never-approved
/// orchestration and fire it with run-now. Nothing spawns, and `schedule list`
/// shows the refusal (`never approved`, the project, the approve command).
/// After `trust approve`, the same run-now spawns the orchestrator.
#[spec("config-trust/schedule/001")]
#[test]
fn schedule_001_an_unapproved_scheduled_orchestration_is_refused_and_reported() {
    let scratch = common::race_safe_tempdir();
    let project = scratch.path().join("scheduled-project");
    std::fs::create_dir_all(&project).expect("create project");
    write_trust_config(&project, CODER_COMMAND, false);
    let toml = format!(
        "[[scheduled_tasks]]\n\
         name = \"trust-sched\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{}\"\n\
         command = \"cat\"\n\
         prompt = \"TRUST-SCHED-PROMPT\"\n\
         enabled = true\n",
        toml_str(&project.to_string_lossy())
    );
    let daemon = common::spawn_daemon_serve_untrusted(Some(&toml), "0", &[]);

    daemon.run_now("trust-sched").expect("run-now");
    let schedule_list = || text(&daemon.run_schedule_cli(&["list"]));
    let reported = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        schedule_list().contains("never approved")
    });
    let listed = schedule_list();
    assert!(
        reported,
        "`schedule list` must show the refused fire:\n{listed}"
    );
    assert_refusal(&listed, &project, "never approved");
    assert!(
        daemon.agent_records().is_empty(),
        "a refused fire spawns nothing: {:?}",
        daemon.agent_records()
    );

    daemon.approve_project_trust(&project);
    daemon
        .run_now("trust-sched")
        .expect("run-now after approval");
    assert!(
        daemon
            .wait_for_agent_where(
                |r| matches!(
                    &r.tab_membership,
                    Some(TabMembership::Orchestration { role_name, .. }) if role_name == "orchestrator"
                ),
                common::load_scaled(Duration::from_secs(15)),
            )
            .is_some(),
        "once approved, the same fire opens the orchestration"
    );
}

// ---------------------------------------------------------------------------
// config-trust/dispatch/001: `dot-agent-deck dispatch --orchestration`
// ---------------------------------------------------------------------------

/// Removes a dispatch worktree (a sibling of the harness tempdir) on drop.
struct SiblingWorktreeGuard(PathBuf);

impl Drop for SiblingWorktreeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dispatch_worktree_of(deck: &TuiDeck, unit: &str) -> PathBuf {
    let work = deck.workdir();
    work.parent()
        .expect("fixture dir has a parent")
        .join(format!(
            "{}-dispatch-{unit}",
            work.file_name()
                .expect("fixture dir has a name")
                .to_string_lossy()
        ))
}

/// Open a plain dashboard pane named `caller` running `cat` and return its
/// pane id.
fn open_caller(deck: &TuiDeck) -> String {
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // confirm current dir -> New Agent form
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t"); // Mode (No mode) -> Name
    deck.send_keys(&[0x7f; 96]);
    deck.send_keys(b"caller");
    deck.send_keys(b"\t"); // Name -> Command
    deck.send_keys(&[0x7f; 400]);
    deck.send_keys(b"cat");
    let (col, row) = deck.wait_for_in_grid("[Submit]");
    deck.click(col, row);
    deck.wait_for_absence("[Submit]");
    let find = || {
        common::agent_records_on(deck.attach_socket_path())
            .into_iter()
            .find(|r| r.display_name.as_deref() == Some("caller"))
            .and_then(|r| r.pane_id_env)
    };
    assert!(
        common::wait_until(common::load_scaled(Duration::from_secs(15)), || find()
            .is_some()),
        "the caller pane never started.\nGrid:\n{}",
        deck.snapshot_grid()
    );
    find().expect("the caller is up")
}

/// Scenario: From a plain pane in a never-approved project, run `dispatch
/// <unit> --orchestration trust-orch`: the refusal reaches the caller's inbox
/// and no worktree is created. After `trust approve` of the main checkout, a second dispatch
/// succeeds and the orchestration's roles run in the unit's linked worktree,
/// which shares the main checkout's approval.
#[spec("config-trust/dispatch/001")]
#[test]
fn dispatch_001_dispatch_orchestration_refuses_until_approved() {
    let deck = TuiDeck::builder()
        .without_trust_preapproval()
        .impersonating_pane_signals()
        .with_pty_size(160, 50)
        .launch_with_fixture("config-trust");
    deck.wait_for_string("No active agents");
    common::commit_fixture_repo(deck.workdir());
    let caller = open_caller(&deck);
    let dispatch = |unit: &str| {
        agent_verb(
            &deck,
            &caller,
            &[
                "dispatch",
                unit,
                "--task",
                "TRUST-DISPATCH-TASK",
                "--orchestration",
                ORCH,
            ],
        )
    };

    let refused_wt = dispatch_worktree_of(&deck, "trust-refused");
    let _g1 = SiblingWorktreeGuard(refused_wt.clone());
    // `dispatch` returns at once and posts its outcome to the caller's inbox
    // (`dispatch_return`); a refusal arrives there, not as an exit status.
    let out = dispatch("trust-refused");
    let attach = deck.attach_socket_path();
    let refused = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        inbox_view::count_containing(attach, &caller, "never approved") > 0
    });
    let inbox = inbox_view::text(attach, &caller);
    assert!(
        refused,
        "the refusal must reach the caller's inbox.\nCLI: {}\nInbox:\n{inbox}",
        describe(&out)
    );
    assert_refusal(&inbox, deck.workdir(), "never approved");
    assert!(
        !refused_wt.exists(),
        "a refused dispatch creates no worktree"
    );
    assert!(
        role_records(deck.attach_socket_path()).is_empty(),
        "a refused dispatch spawns nothing"
    );

    deck.approve_project_trust(deck.workdir());
    let unit_wt = dispatch_worktree_of(&deck, "trust-approved");
    let _g2 = SiblingWorktreeGuard(unit_wt.clone());
    let out = dispatch("trust-approved");
    assert!(
        out.status.success(),
        "an approved dispatch runs: {}",
        describe(&out)
    );
    let unit_name = unit_wt
        .file_name()
        .expect("worktree has a name")
        .to_string_lossy()
        .into_owned();
    let in_unit = common::wait_until(common::load_scaled(Duration::from_secs(20)), || {
        role_records(deck.attach_socket_path()).iter().any(|r| {
            r.cwd
                .as_deref()
                .and_then(|cwd| Path::new(cwd).file_name().map(|n| n == unit_name.as_str()))
                .unwrap_or(false)
        })
    });
    assert!(
        in_unit,
        "the approved dispatch's roles must run in its worktree {}; roles = {:?}",
        unit_wt.display(),
        role_records(deck.attach_socket_path())
            .iter()
            .map(|r| (&r.cwd, &r.tab_membership))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// config-trust/schedule/003: a forged `.git` file borrows no approval
// ---------------------------------------------------------------------------

/// A two-role orchestration `team` whose start role runs the project's own
/// `./agent.sh` — the same TOML text in every project that uses it.
const AGENT_SCRIPT_CONFIG: &str = "[[orchestrations]]\n\
     name = \"team\"\n\n\
     [[orchestrations.roles]]\n\
     name = \"orchestrator\"\n\
     command = \"sh ./agent.sh\"\n\
     start = true\n\n\
     [[orchestrations.roles]]\n\
     name = \"worker\"\n\
     command = \"cat\"\n";

/// Scenario: Approve repository A's `team` (command `sh ./agent.sh`) and cut a
/// legitimate linked worktree of it. Directory B holds the same TOML, a
/// different `agent.sh` that writes a marker, and a `.git` file copied from A's
/// worktree. A run-now schedule in B must be refused and never write B's
/// marker, while one in A's real linked worktree still opens the orchestration.
#[spec("config-trust/schedule/003")]
#[test]
fn schedule_003_a_forged_gitfile_does_not_borrow_another_checkouts_approval() {
    let scratch = common::race_safe_tempdir();
    let root = scratch.path().to_path_buf();
    let repo_a = root.join("approved-repo");
    let worktree_a = root.join("approved-repo-wt");
    let forged_b = root.join("forged-project");
    for dir in [&repo_a, &forged_b] {
        std::fs::create_dir_all(dir).expect("create sandbox dir");
    }
    std::fs::write(repo_a.join(".dot-agent-deck.toml"), AGENT_SCRIPT_CONFIG).expect("A config");
    std::fs::write(repo_a.join("agent.sh"), "exec cat\n").expect("A agent.sh");
    for args in [
        &["-c", "init.defaultBranch=main", "init", "-q"][..],
        &["add", "-A"],
        &["commit", "-q", "-m", "init"],
    ] {
        let status = common::fixture_git(&repo_a, &root)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in repository A");
    }
    let added = common::fixture_git(&repo_a, &root)
        .args(["worktree", "add", "-q", "-b", "wt"])
        .arg(&worktree_a)
        .status()
        .expect("git worktree add");
    assert!(added.success(), "git worktree add failed");

    // B: byte-identical TOML, a different script, A's worktree's `.git` file.
    std::fs::write(forged_b.join(".dot-agent-deck.toml"), AGENT_SCRIPT_CONFIG).expect("B config");
    std::fs::write(
        forged_b.join("agent.sh"),
        "echo forged > hostile-marker\nexec cat\n",
    )
    .expect("B agent.sh");
    std::fs::copy(worktree_a.join(".git"), forged_b.join(".git")).expect("copy the .git file");
    let marker = forged_b.join("hostile-marker");

    let schedule = |name: &str, dir: &Path| {
        format!(
            "[[scheduled_tasks]]\n\
             name = \"{name}\"\n\
             cron = \"0 0 1 1 *\"\n\
             working_dir = \"{}\"\n\
             command = \"cat\"\n\
             prompt = \"TRUST-FORGED-PROMPT\"\n\
             enabled = true\n",
            toml_str(&dir.to_string_lossy())
        )
    };
    let toml = format!(
        "{}\n{}",
        schedule("forged-sched", &forged_b),
        schedule("legit-sched", &worktree_a)
    );
    let daemon = common::spawn_daemon_serve_untrusted(Some(&toml), "0", &[]);
    daemon.approve_project_trust(&repo_a);

    daemon.run_now("forged-sched").expect("run-now in B");
    let forged_canonical = canonical(&forged_b);
    let schedule_list = || text(&daemon.run_schedule_cli(&["list"]));
    let settled = common::wait_until(common::load_scaled(Duration::from_secs(15)), || {
        marker.exists() || {
            let listed = schedule_list();
            listed.contains("dot-agent-deck trust approve") && listed.contains(&forged_canonical)
        }
    });
    let in_b = |r: &AgentRecord| {
        r.cwd.as_deref().is_some_and(|cwd| {
            std::fs::canonicalize(cwd).is_ok_and(|c| c.to_string_lossy() == forged_canonical)
        })
    };
    assert!(
        !marker.exists() && !common::wait_until(Duration::from_secs(2), || marker.exists()),
        "B was never approved, but its run-now ran B's own agent.sh (marker {} written): a \
         `.git` file copied from A's linked worktree borrowed A's approval.\nAgents in B: {:?}\n\
         schedule list:\n{}",
        marker.display(),
        daemon
            .agent_records()
            .iter()
            .filter(|r| in_b(r))
            .map(|r| &r.tab_membership)
            .collect::<Vec<_>>(),
        schedule_list()
    );
    assert!(
        settled,
        "B's refused fire must be reported in `schedule list`, with the approve command naming \
         B:\n{}",
        schedule_list()
    );
    assert!(
        !daemon.agent_records().iter().any(in_b),
        "a refused fire in B spawns nothing there: {:?}",
        daemon.agent_records()
    );

    // Control: A's legitimate linked worktree still shares A's approval.
    let worktree_canonical = canonical(&worktree_a);
    daemon
        .run_now("legit-sched")
        .expect("run-now in A's worktree");
    assert!(
        daemon
            .wait_for_agent_where(
                |r| matches!(
                    &r.tab_membership,
                    Some(TabMembership::Orchestration { role_name, .. }) if role_name == "orchestrator"
                ) && r.cwd.as_deref().is_some_and(|cwd| {
                    std::fs::canonicalize(cwd)
                        .is_ok_and(|c| c.to_string_lossy() == worktree_canonical)
                }),
                common::load_scaled(Duration::from_secs(15)),
            )
            .is_some(),
        "A's real linked worktree must still share A's approval and open the orchestration.\n\
         schedule list:\n{}",
        schedule_list()
    );
}

// ---------------------------------------------------------------------------
// config-trust/rearm/001: a warm reattach re-arms only approved instructions
// ---------------------------------------------------------------------------

/// The start role's instructions after the unapproved edit.
const REARM_SENTINEL: &str = "UNAPPROVED_REARM_SENTINEL_9c41";

fn write_rearm_config(project: &Path, start_prompt: &str) {
    let toml = format!(
        "[[orchestrations]]\n\
         name = \"rearm-orch\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"cat\"\n\
         start = true\n\
         description = \"REARM-DESC coordinates the stand-ins\"\n\
         prompt_template = \"{start_prompt}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"worker\"\n\
         command = \"cat\"\n"
    );
    std::fs::write(project.join(".dot-agent-deck.toml"), toml).expect("write the re-arm config");
}

/// Every orchestrator context file the deck published under `project`, with
/// its content.
fn published_context_files(project: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(project.join(".dot-agent-deck")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("orchestrator-context"))
        })
        .filter_map(|p| {
            let content = String::from_utf8_lossy(&std::fs::read(&p).ok()?).into_owned();
            Some((p, content))
        })
        .collect()
}

/// Where the sentinel has been published, if anywhere: a context file under
/// `project` or a message in the start role's inbox.
fn sentinel_published(project: &Path, attach: &Path, start_pane: &str) -> Option<String> {
    if let Some((path, _)) = published_context_files(project)
        .into_iter()
        .find(|(_, content)| content.contains(REARM_SENTINEL))
    {
        return Some(format!("context file {}", path.display()));
    }
    (inbox_view::count_containing(attach, start_pane, REARM_SENTINEL) > 0).then(|| {
        format!(
            "the start role's inbox:\n{}",
            inbox_view::text(attach, start_pane)
        )
    })
}

/// Inject one synthetic hook event for `pane_id` / `agent_id` over `hook_socket`.
fn inject_event(
    hook_socket: &Path,
    pane_id: &str,
    agent_id: &str,
    event_type: dot_agent_deck::event::EventType,
    agent_type: dot_agent_deck::event::AgentType,
    tool_name: Option<&str>,
    metadata: std::collections::HashMap<String, String>,
) {
    let event = dot_agent_deck::event::AgentEvent {
        session_id: format!("{agent_id}-rearm-session"),
        agent_type,
        event_type,
        tool_name: tool_name.map(str::to_string),
        tool_detail: None,
        cwd: None,
        timestamp: chrono::Utc::now(),
        user_prompt: None,
        metadata,
        pane_id: Some(pane_id.to_string()),
        agent_id: Some(agent_id.to_string()),
        agent_version: None,
        schema_version: None,
        live_target: None,
        background: None,
        background_revision: None,
    };
    let line = serde_json::to_string(&event).expect("serialize the synthetic event");
    common::write_hook_line(hook_socket, &line).expect("inject the synthetic event");
}

/// Wait until the daemon's live status for `pane_id` satisfies `pred`.
fn wait_for_live(
    attach: &Path,
    pane_id: &str,
    pred: impl Fn(&dot_agent_deck::state::SessionSnapshot) -> bool,
) -> bool {
    common::wait_until(common::load_scaled(Duration::from_secs(10)), || {
        common::agent_records_on(attach).into_iter().any(|r| {
            r.pane_id_env.as_deref() == Some(pane_id) && r.live.as_ref().is_some_and(&pred)
        })
    })
}

/// Scenario: A schedule opens an approved stand-in orchestration; then only its
/// start role's `prompt_template` is edited to carry a sentinel, and the TUI
/// attaches to the warm daemon. A compaction and then a `/clear` on the start
/// role must re-arm nothing carrying the sentinel: no published context file and
/// no start-role inbox message contains it while the approval reads `changed`.
#[spec("config-trust/rearm/001")]
#[test]
fn rearm_001_a_warm_reattach_never_publishes_changed_instructions() {
    use dot_agent_deck::event::{
        AgentType, CLEAR_SESSION_START_METADATA_KEY, CLEAR_SESSION_START_METADATA_VALUE, EventType,
    };
    let scratch = common::race_safe_tempdir();
    let project = scratch.path().join("rearm-project");
    std::fs::create_dir_all(&project).expect("create project");
    write_rearm_config(&project, "REARM-APPROVED-PROMPT coordinate the stand-ins");
    let toml = format!(
        "[[scheduled_tasks]]\n\
         name = \"rearm-sched\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{}\"\n\
         command = \"cat\"\n\
         prompt = \"REARM-SCHED-PROMPT\"\n\
         enabled = true\n",
        toml_str(&project.to_string_lossy())
    );
    // Pre-approves the schedule's project, as a user who approved it would.
    let daemon = common::spawn_daemon_serve_with_env(
        Some(&toml),
        "0",
        &[(
            dot_agent_deck::hook_provenance::DOT_AGENT_DECK_HOOK_PROVENANCE,
            "warn",
        )],
    );
    daemon.run_now("rearm-sched").expect("run-now");
    let start = daemon
        .wait_for_agent_where(
            |r| matches!(
                &r.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == "orchestrator"
            ),
            common::load_scaled(Duration::from_secs(15)),
        )
        .expect("precondition: the approved scheduled orchestration opens its start role");
    let start_pane = start
        .pane_id_env
        .clone()
        .expect("the start role has a pane id");
    let start_agent = start.id.clone();

    // Only the start role's instructions change; nothing approves the edit.
    write_rearm_config(
        &project,
        &format!("{REARM_SENTINEL} do something else entirely"),
    );
    let listed = text(&daemon.run_deck_cli(&project, &["trust", "list"]));
    assert!(
        listed
            .lines()
            .any(|l| l.contains("rearm-orch") && state_word(l) == Some("changed")),
        "precondition: the approval reads `changed` after the edit:\n{listed}"
    );

    // Warm reattach: a TUI attaches to the daemon already running the roles.
    let deck = TuiDeck::builder()
        .with_pty_size(160, 40)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon.hook_socket.to_string_lossy().to_string(),
        )
        .launch_with_fixture("minimal");
    deck.wait_until_grid("the reattached orchestration tab is in the strip", |grid| {
        grid.lines()
            .next()
            .is_some_and(|tabs| tabs.contains("rearm-orch"))
    });
    let attach = daemon.attach_socket.clone();

    // Re-arm 1: compaction on the start role.
    inject_event(
        &daemon.hook_socket,
        &start_pane,
        &start_agent,
        EventType::Compacting,
        AgentType::Codex,
        None,
        Default::default(),
    );
    assert!(
        wait_for_live(&attach, &start_pane, |s| {
            s.status == dot_agent_deck::state::SessionStatus::Compacting
        }),
        "precondition: the daemon applies the start role's Compacting"
    );
    let leaked = common::wait_until(Duration::from_secs(5), || {
        sentinel_published(&project, &attach, &start_pane).is_some()
    });
    assert!(
        !leaked,
        "a compaction re-arm after a warm reattach published the unapproved start-role \
         instructions ({REARM_SENTINEL}) to {} while the approval reads `changed`.\nGrid:\n{}",
        sentinel_published(&project, &attach, &start_pane).unwrap_or_default(),
        deck.snapshot_grid()
    );

    // Re-arm 2: `/clear` on the start role (prime a tool so the clear's own
    // footprint — the tool cleared — is observable).
    let primer = "rearm-clear-primer";
    inject_event(
        &daemon.hook_socket,
        &start_pane,
        &start_agent,
        EventType::ToolStart,
        AgentType::ClaudeCode,
        Some(primer),
        Default::default(),
    );
    assert!(
        wait_for_live(&attach, &start_pane, |s| {
            s.active_tool.as_ref().is_some_and(|t| t.name == primer)
        }),
        "precondition: the daemon applies the primer tool"
    );
    inject_event(
        &daemon.hook_socket,
        &start_pane,
        &start_agent,
        EventType::SessionStart,
        AgentType::ClaudeCode,
        None,
        [(
            CLEAR_SESSION_START_METADATA_KEY.to_string(),
            CLEAR_SESSION_START_METADATA_VALUE.to_string(),
        )]
        .into_iter()
        .collect(),
    );
    assert!(
        wait_for_live(&attach, &start_pane, |s| s.active_tool.is_none()),
        "precondition: the daemon applies the start role's /clear SessionStart"
    );
    let leaked = common::wait_until(Duration::from_secs(5), || {
        sentinel_published(&project, &attach, &start_pane).is_some()
    });
    assert!(
        !leaked,
        "a /clear re-arm after a warm reattach published the unapproved start-role \
         instructions ({REARM_SENTINEL}) to {} while the approval reads `changed`.\nGrid:\n{}",
        sentinel_published(&project, &attach, &start_pane).unwrap_or_default(),
        deck.snapshot_grid()
    );
}

// ---------------------------------------------------------------------------
// config-trust/escape/001: refusal text carries no raw terminal controls
// ---------------------------------------------------------------------------

/// An orchestration name a hostile project could choose: an ESC sequence that
/// clears the screen and a right-to-left override, written with TOML escapes.
const HOSTILE_ORCH_TOML: &str = "esc\\u001b[2JTRUST-AUDIT\\u202eflip";
/// A role name with an OSC-8 hyperlink opener and a right-to-left override.
const HOSTILE_ROLE_TOML: &str = "rev\\u001b]8;;http://x\\u0007\\u202eiew";

/// A project whose one orchestration carries the hostile name. Spawn
/// validation refuses such a name, so it is only ever refused, never opened.
fn write_hostile_orchestration_config(dir: &Path) {
    let toml = format!(
        "[[orchestrations]]\n\
         name = \"{HOSTILE_ORCH_TOML}\"\n\n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"env TRUST_ROLE=orchestrator cat\"\n\
         start = true\n"
    );
    std::fs::write(dir.join(".dot-agent-deck.toml"), toml).expect("write the hostile config");
}

/// The `config-trust` orchestration with `coder` running `coder_command`, plus
/// a role carrying the hostile name.
fn write_trust_config_with_hostile_role(dir: &Path, coder_command: &str) {
    write_trust_config(dir, coder_command, false);
    let path = dir.join(".dot-agent-deck.toml");
    let mut toml = std::fs::read_to_string(&path).expect("read the trust config");
    toml.push_str(&format!(
        "\n[[orchestrations.roles]]\n\
         name = \"{HOSTILE_ROLE_TOML}\"\n\
         command = \"cat\"\n"
    ));
    std::fs::write(&path, toml).expect("write the trust config");
}

/// The raw terminal controls a refusal must never carry: ESC, BEL and the
/// bidi overrides/isolates.
fn raw_controls(text: &str) -> Vec<String> {
    text.chars()
        .filter(|c| {
            matches!(
                c,
                '\u{1b}' | '\u{7}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
        })
        .map(|c| format!("U+{:04X}", c as u32))
        .collect()
}

/// Run `dot-agent-deck <args…>` against `daemon` as the agent in `caller_pane`.
fn daemon_agent_verb(
    daemon: &common::DaemonProc,
    cwd: &Path,
    caller_pane: &str,
    args: &[&str],
) -> Output {
    std::process::Command::new(common::contained_deck_binary())
        .args(args)
        .env("DOT_AGENT_DECK_SOCKET", &daemon.hook_socket)
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", &daemon.attach_socket)
        .env("DOT_AGENT_DECK_PANE_ID", caller_pane)
        .env("HOME", &daemon.home)
        .current_dir(cwd)
        .output()
        .expect("run the agent verb")
}

/// Scenario: A project names its orchestration with an ESC clear-screen
/// sequence and a bidi override; a run-now refusal must leave no raw ESC or
/// bidi character in the daemon's stderr or log. A second, approved and open
/// orchestration then gains a role with a hostile name, and the refused
/// `delegate` and `pane restart` print no raw ESC, BEL or bidi character.
#[spec("config-trust/escape/001")]
#[test]
fn escape_001_refusals_carry_no_raw_terminal_controls() {
    let scratch = common::race_safe_tempdir();
    let hostile = scratch.path().join("hostile-project");
    let roles = scratch.path().join("roles-project");
    for dir in [&hostile, &roles] {
        std::fs::create_dir_all(dir).expect("create project");
    }
    write_hostile_orchestration_config(&hostile);
    write_trust_config(&roles, CODER_COMMAND, false);
    let log = scratch.path().join("daemon.log");
    let schedule = |name: &str, dir: &Path| {
        format!(
            "[[scheduled_tasks]]\n\
             name = \"{name}\"\n\
             cron = \"0 0 1 1 *\"\n\
             working_dir = \"{}\"\n\
             command = \"cat\"\n\
             prompt = \"TRUST-HOSTILE-PROMPT\"\n\
             enabled = true\n",
            toml_str(&dir.to_string_lossy())
        )
    };
    let toml = format!(
        "{}\n{}",
        schedule("hostile-sched", &hostile),
        schedule("roles-sched", &roles)
    );
    let log_arg = log.to_string_lossy().into_owned();
    let daemon = common::spawn_daemon_serve_untrusted(
        Some(&toml),
        "0",
        &[
            ("DOT_AGENT_DECK_LOG", log_arg.as_str()),
            (
                dot_agent_deck::hook_provenance::DOT_AGENT_DECK_HOOK_PROVENANCE,
                "warn",
            ),
        ],
    );

    // 1. The scheduled fire is refused; its diagnostics stay inert text.
    daemon.run_now("hostile-sched").expect("run-now");
    assert!(
        daemon.wait_for_stderr_contains("refused", common::load_scaled(Duration::from_secs(15))),
        "precondition: the daemon reports the refused fire on stderr"
    );
    assert!(
        daemon.stderr_contains("TRUST-AUDIT"),
        "precondition: the refusal on stderr names the orchestration"
    );
    let log_text = String::from_utf8_lossy(&std::fs::read(&log).unwrap_or_default()).into_owned();
    for (sink, found) in [
        (
            "daemon stderr",
            ["\u{1b}", "\u{202e}"]
                .iter()
                .filter(|c| daemon.stderr_contains(c))
                .map(|c| format!("U+{:04X}", c.chars().next().unwrap_or_default() as u32))
                .collect::<Vec<_>>(),
        ),
        ("daemon log", raw_controls(&log_text)),
    ] {
        assert!(
            found.is_empty(),
            "a refused fire wrote raw terminal controls {found:?} from the project's \
             orchestration name to the {sink}; they must be escaped"
        );
    }

    // 2. Agent verbs: approve and open a benign orchestration (spawn validation
    // refuses a hostile orchestration name), then add a hostile-named role.
    daemon.approve_project_trust(&roles);
    daemon
        .run_now("roles-sched")
        .expect("run-now after approval");
    let orchestrator = daemon
        .wait_for_agent_where(
            |r| matches!(
                &r.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == "orchestrator"
            ),
            common::load_scaled(Duration::from_secs(15)),
        )
        .and_then(|r| r.pane_id_env)
        .expect("precondition: the approved orchestration opens its start role");
    write_trust_config_with_hostile_role(&roles, CODER_COMMAND_EDITED);

    for args in [
        &["delegate", "--to", "coder", "--task", "TRUST-HOSTILE-TASK"][..],
        &["pane", "restart", "coder", "--force"],
    ] {
        let out = daemon_agent_verb(&daemon, &roles, &orchestrator, args);
        let printed = text(&out);
        assert!(
            !out.status.success()
                && printed.contains("changed since you approved it")
                && printed.contains("http://x"),
            "precondition: `{}` is refused after the change, naming the added role: {}",
            args.join(" "),
            describe(&out).escape_debug()
        );
        let found = raw_controls(&printed);
        assert!(
            found.is_empty(),
            "the refused `{}` printed raw terminal controls {found:?} from the project's \
             role names; they must be escaped.\n{}",
            args.join(" "),
            printed.escape_debug()
        );
    }
}
