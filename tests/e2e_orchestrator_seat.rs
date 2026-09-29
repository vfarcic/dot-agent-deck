#![cfg(all(feature = "e2e", unix))]

//! L2 coverage for WHICH role of a `Ctrl+n` orchestration tab is its
//! orchestrator (issue #523).
//!
//! One question — "who is the orchestrator?" — decides four user-visible
//! things on a tab: which role's pane has focus when the tab opens, which pane
//! the orchestrator prompt is delivered into, which pane the daemon lets
//! `delegate`, and which card `daemon status` marks `(orchestrator)`. Before
//! #523 the `Ctrl+n` path answered it from the bare `start = true` flag and
//! fell back to role 0 when no role set it, while the daemon's dispatched and
//! scheduled spawn path fell back to the role NAMED `orchestrator` first. So a
//! config whose orchestrator is named `orchestrator` but never says
//! `start = true` opened with focus and the orchestrator prompt on whichever
//! role happened to come first, and the named orchestrator's own `delegate`
//! was refused. Both paths now read one rule,
//! `OrchestrationConfig::orchestrator_role_index`.
//!
//! The roles are `tee` stand-ins that print a ready sentinel and then copy
//! whatever reaches their PTY both back to the pane and into a per-role file,
//! so "which pane received the bytes" is a plain substring question about that
//! file. The file, not the rendered pane, is asserted on because the prompt
//! delivery's own control bytes are echoed raw and redraw the pane. They stand
//! in for a real agent only in that they are processes in a pane; nothing here
//! depends on an agent reading anything.
//! The config deliberately puts the worker FIRST, so the pre-#523 role-0
//! fallback lands on a pane that is visibly the wrong one.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::{AgentRecord, TabMembership};
use spec::spec;

/// Every byte a stand-in receives is copied by `tee`, raw — no line
/// discipline, no local echo — to the pane and to `<role>.in` in the workdir.
fn write_standin(dir: &std::path::Path, role: &str, sentinel: &str) {
    let path = dir.join(format!("seat-{role}.sh"));
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nstty -echo -icanon -icrnl -opost min 1 time 0\nprintf '{sentinel}\\n'\nexec tee -a {role}.in\n"
        ),
    )
    .expect("write stand-in");
    let mut perms = std::fs::metadata(&path)
        .expect("stat stand-in")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod stand-in");
}

/// Two roles, worker first. `start` says whether the `orchestrator` role
/// carries `start = true`; the role is named `orchestrator` either way.
fn write_config(deck: &TuiDeck, start: bool) {
    write_standin(deck.workdir(), "coder", "CODER-READY");
    write_standin(deck.workdir(), "orchestrator", "ORCH-READY");
    let start_line = if start { "start = true\n" } else { "" };
    let config = format!(
        "[[orchestrations]]\nname = \"seat\"\n\n\
         [[orchestrations.roles]]\nname = \"coder\"\ncommand = \"./seat-coder.sh\"\n\
         description = \"writes code\"\nclear = false\n\n\
         [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"./seat-orchestrator.sh\"\n\
         prompt_template = \"SEAT-TEMPLATE-SENTINEL\"\nclear = false\n{start_line}"
    );
    std::fs::write(deck.workdir().join(".dot-agent-deck.toml"), config)
        .expect("write orchestration config");
}

fn role_agent(deck: &TuiDeck, role: &str) -> AgentRecord {
    common::agent_records_on(deck.attach_socket_path())
        .into_iter()
        .find(|agent| {
            matches!(
                &agent.tab_membership,
                Some(TabMembership::Orchestration { role_name, .. }) if role_name == role
            )
        })
        .unwrap_or_else(|| panic!("no `{role}` role agent registered"))
}

/// Everything `role`'s process has read from its PTY so far.
fn received(deck: &TuiDeck, role: &str) -> String {
    std::fs::read_to_string(deck.workdir().join(format!("{role}.in"))).unwrap_or_default()
}

/// The first complete `orchestrator-context-<32 hex>.md` name in `text` — the
/// per-tab context file a delivered pointer names (issue #1233).
fn context_file_named(text: &str) -> Option<String> {
    const PREFIX: &str = "orchestrator-context-";
    text.match_indices(PREFIX).find_map(|(at, _)| {
        let rest = &text[at + PREFIX.len()..];
        let hex = rest.get(..32)?;
        (hex.bytes().all(|b| b.is_ascii_hexdigit()) && rest[32..].starts_with(".md"))
            .then(|| format!("{PREFIX}{hex}.md"))
    })
}

/// Run the real `delegate` CLI as `from`'s pane. The test process is not that
/// pane, hence `impersonating_pane_signals` on the deck.
fn delegate_from(deck: &TuiDeck, from: &str, to: &str, task: &str) -> Output {
    let pane = role_agent(deck, from);
    Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["delegate", "--to", to, "--task", task])
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env(
            "DOT_AGENT_DECK_PANE_ID",
            pane.pane_id_env.expect("role pane id"),
        )
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run delegate CLI")
}

/// The `role` labels `daemon status --json` prints for this deck's agents.
fn status_roles(deck: &TuiDeck) -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["daemon", "status", "--json"])
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", deck.attach_socket_path())
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run daemon status --json");
    assert!(
        output.status.success(),
        "daemon status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    doc["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .filter_map(|a| a["role"].as_str().map(str::to_string))
        .collect()
}

/// Open the config's only orchestration through the new-pane form, exactly as
/// a person does: `Ctrl+n`, confirm the directory, Right onto the
/// orchestration chip, Enter past the name, Enter to submit.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_bytes(b"\x0e");
    deck.send_bytes(b" ");
    deck.wait_for_string("No mode");
    deck.send_bytes(b"\x1b[C");
    deck.send_bytes(b"\r");
    deck.send_bytes(b"\r");
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            let roles: Vec<String> = common::agent_records_on(deck.attach_socket_path())
                .into_iter()
                .filter_map(|a| match a.tab_membership {
                    Some(TabMembership::Orchestration { role_name, .. }) => Some(role_name),
                    _ => None,
                })
                .collect();
            roles.iter().any(|r| r == "coder") && roles.iter().any(|r| r == "orchestrator")
        }),
        "both role panes should be registered after opening the tab"
    );
}

/// Scenario: Launch the real TUI, write a two-role orchestration whose worker
/// `coder` comes first and whose second role is named `orchestrator`, and open
/// it with `Ctrl+n` — once with `start = true` on `orchestrator` and once with
/// no `start` anywhere. In both, what the user types lands in `orchestrator`'s
/// pane, the orchestrator prompt is delivered there and nowhere else, it is the
/// one card `daemon status` marks `(orchestrator)`, and its `delegate` reaches
/// `coder` while `coder`'s own `delegate` is refused.
#[spec("tabs/orchestration/001")]
#[test]
fn tabs_orchestration_001_ctrl_n_tab_seats_one_orchestrator() {
    // Control first: `start = true` on the named role is the documented
    // config, and passed before #523 too — so if it goes red, the harness is
    // what broke, not the rule.
    for (case, start) in [
        ("start = true on the orchestrator", true),
        ("no start = true, role named orchestrator", false),
    ] {
        let deck = TuiDeck::builder()
            .with_pty_size(160, 45)
            .impersonating_pane_signals()
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active agents");
        write_config(&deck, start);
        open_orchestration(&deck);

        // Default focus: a keystroke in PaneInput goes to the focused pane.
        let probe = "FOCUS-PROBE-523";
        deck.send_bytes(probe.as_bytes());
        assert!(
            common::wait_until(Duration::from_secs(10), || received(&deck, "orchestrator")
                .contains(probe)),
            "[{case}] typing on the freshly opened tab should reach `orchestrator`'s pane.\n\
             orchestrator received:\n{}\ncoder received:\n{}",
            received(&deck, "orchestrator"),
            received(&deck, "coder"),
        );
        assert!(
            !received(&deck, "coder").contains(probe),
            "[{case}] the keystroke must not also reach `coder`"
        );

        // Orchestrator prompt: the pointer to the context file lands in the
        // orchestrator's pane. A `cat` stand-in never reports SessionStart, so
        // this waits out the delivery fallback. Each tab's context is its own
        // `orchestrator-context-<32 hex>.md` (issue #1233), so the prefix is
        // what is waited for and the file read is the one the pointer names.
        let pointer = "orchestrator-context-";
        assert!(
            common::wait_until(Duration::from_secs(45), || context_file_named(&received(
                &deck,
                "orchestrator"
            ))
            .is_some()),
            "[{case}] the orchestrator prompt should be delivered into `orchestrator`'s pane.\n\
             orchestrator received:\n{}\ncoder received:\n{}",
            received(&deck, "orchestrator"),
            received(&deck, "coder"),
        );
        assert!(
            !received(&deck, "coder").contains(pointer),
            "[{case}] the orchestrator prompt must not reach `coder`:\n{}",
            received(&deck, "coder")
        );
        // What the pointer points at: the orchestrator's own template, and
        // `coder` — not the orchestrator itself — offered as the team.
        let named = context_file_named(&received(&deck, "orchestrator"))
            .expect("the pointer names a context file");
        let context = std::fs::read_to_string(deck.workdir().join(".dot-agent-deck").join(&named))
            .unwrap_or_else(|e| panic!("[{case}] read the pointed-at {named}: {e}"));
        assert!(
            context.contains("SEAT-TEMPLATE-SENTINEL"),
            "[{case}] the context should open with the orchestrator's prompt_template:\n{context}"
        );
        assert!(
            context.contains("**coder**") && !context.contains("**orchestrator**"),
            "[{case}] the context's available agents should be the workers only:\n{context}"
        );

        // Exactly one card is the orchestrator, and it is the named one.
        let roles = status_roles(&deck);
        let marked: Vec<&String> = roles
            .iter()
            .filter(|r| r.contains("(orchestrator)"))
            .collect();
        assert_eq!(
            marked,
            vec!["orchestrator (orchestrator)"],
            "[{case}] exactly one card should be marked the orchestrator; roles: {roles:?}"
        );

        // Delegate permission: the orchestrator may, the worker may not.
        let refused = delegate_from(&deck, "coder", "orchestrator", "worker-must-not");
        assert!(
            !refused.status.success(),
            "[{case}] `coder` is a worker and its delegate must be refused; stderr: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
        let accepted = delegate_from(&deck, "orchestrator", "coder", "seat-delegate-523");
        assert!(
            accepted.status.success(),
            "[{case}] the orchestrator's delegate must be accepted; stderr: {}",
            String::from_utf8_lossy(&accepted.stderr)
        );
        assert!(
            common::wait_until(Duration::from_secs(15), || received(&deck, "coder")
                .contains("worker-task-coder")),
            "[{case}] the accepted delegate should write the task pointer into `coder`:\n{}",
            received(&deck, "coder")
        );
    }
}
