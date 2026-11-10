//! L1 tests for the TUI's project-trust prompt (PRD #1885): the `Review
//! project commands` popup drawn by `trust_prompt::render_to_buffer` from the
//! view the daemon answers with, built here the way the daemon builds it
//! (`TrustCheck::evaluate(..).view()` against an approval store).
//!
//! The spawned-binary path — the New Agent form and session restore opening
//! the prompt, `a` approving and `Esc` cancelling — is `tests/e2e_config_trust.rs`
//! (`config-trust/prompt/*`, `config-trust/restore/*`).

use std::path::{Path, PathBuf};

use dot_agent_deck::project_config::{OrchestrationConfig, OrchestrationRoleConfig};
use dot_agent_deck::project_trust::{ApprovalStore, ProjectTrustView, TrustCheck, role_texts};
use dot_agent_deck::trust_prompt::{
    CLEAR_FALSE, CLEAR_LABEL, CLEAR_TRUE, PromptAction, START_FALSE, START_LABEL, START_TRUE,
    render_to_buffer,
};
use spec::spec;

const PROJECT: &str = "/work/demo-project";

fn role(name: &str, command: &str, prompt: &str) -> OrchestrationRoleConfig {
    OrchestrationRoleConfig {
        name: name.to_string(),
        command: command.to_string(),
        agent: None,
        start: name == "orchestrator",
        description: Some(format!("The {name} of the team")),
        prompt_template: Some(prompt.to_string()),
        clear: false,
    }
}

fn orchestration(roles: Vec<OrchestrationRoleConfig>) -> OrchestrationConfig {
    OrchestrationConfig {
        default: false,
        name: "team".to_string(),
        roles,
    }
}

/// What the daemon shows for `current`, given what (if anything) was approved.
fn view(approved: Option<&OrchestrationConfig>, current: &OrchestrationConfig) -> ProjectTrustView {
    let project = PathBuf::from(PROJECT);
    let mut store = ApprovalStore::default();
    if let Some(approved) = approved {
        store.approve(
            &project,
            &approved.name,
            role_texts(approved),
            chrono::Utc::now(),
        );
    }
    TrustCheck::evaluate(&store, project, Path::new(PROJECT), current).view()
}

fn text(view: &ProjectTrustView, action: PromptAction, scroll: u16, w: u16, h: u16) -> String {
    let buffer = render_to_buffer(view, action, scroll, w, h);
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rows of `screen` that belong to role `name`: its `Role:` row and every
/// row after it up to the next role, the removed-roles line or the footer.
fn role_block(screen: &str, name: &str) -> String {
    let head = format!("Role: {name}");
    let mut lines = screen.lines().skip_while(|l| !l.contains(&head));
    let first = lines
        .next()
        .unwrap_or_else(|| panic!("role {name:?} is not listed:\n{screen}"));
    std::iter::once(first)
        .chain(lines.take_while(|l| {
            !l.contains("Role: ") && !l.contains("Removed since") && !l.contains("a approve")
        }))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The row the prompt shows for a flag: `<label>: <value>`, in the words both
/// clients share (`src/trust_prompt.rs`).
fn flag_row(label: &str, value: &str) -> String {
    format!("{label}: {value}")
}

/// Assert role `name`'s block in `screen` shows `start` and `clear` as the
/// given values, and not the opposite ones.
fn assert_flags(screen: &str, name: &str, start: bool, clear: bool) {
    let block = role_block(screen, name);
    let (start_shown, start_other) = if start {
        (START_TRUE, START_FALSE)
    } else {
        (START_FALSE, START_TRUE)
    };
    let (clear_shown, clear_other) = if clear {
        (CLEAR_TRUE, CLEAR_FALSE)
    } else {
        (CLEAR_FALSE, CLEAR_TRUE)
    };
    for (label, want, other) in [
        (START_LABEL, start_shown, start_other),
        (CLEAR_LABEL, clear_shown, clear_other),
    ] {
        let want = flag_row(label, want);
        assert!(
            block
                .lines()
                .any(|l| l.trim_start_matches('│').trim_start().starts_with(&want)),
            "role {name:?} must show {want:?}.\nRole block:\n{block}\nWhole prompt:\n{screen}"
        );
        let other = flag_row(label, other);
        assert!(
            !block
                .lines()
                .any(|l| l.trim_start_matches('│').trim_start().starts_with(&other)),
            "role {name:?} must not show {other:?}.\nRole block:\n{block}"
        );
    }
}

/// The approved orchestration the "changed" cases start from: three roles.
fn approved_team() -> OrchestrationConfig {
    orchestration(vec![
        role("orchestrator", "claude", "Coordinate the team."),
        role("coder", "claude --model sonnet", "Write the code."),
        role("reviewer", "codex", "Review every diff."),
    ])
}

/// The same orchestration after an edit: `coder`'s command changed, `tester`
/// was added and `reviewer` removed.
fn changed_team() -> OrchestrationConfig {
    orchestration(vec![
        role("orchestrator", "claude", "Coordinate the team."),
        role("coder", "claude --model opus", "Write the code."),
        role("tester", "opencode", "Write the failing test first."),
    ])
}

/// Scenario: Draw the trust prompt for an orchestration that was never approved, as the New Agent form opens it. The popup must be titled `Review project commands`, say `not approved before`, list every role's command, description, prompt and `start` / `clear` values with no role marked, and end with the `a approve and start · Esc cancel · ↑↓ scroll` footer and no "more" hint, since everything fits.
#[spec("config-trust/prompt/003")]
#[test]
fn prompt_003_trust_prompt_render_snapshots() {
    // 1. Never approved, from the New Agent form.
    let team = orchestration(vec![
        role("orchestrator", "claude", "Coordinate the team."),
        role("coder", "claude --model sonnet", "Write the code."),
    ]);
    let never = text(&view(None, &team), PromptAction::Start, 0, 100, 40);
    for want in [
        "Review project commands",
        &format!("Project: {PROJECT}"),
        "Orchestration: team",
        "State: not approved before",
        "Starting it runs these commands and gives these prompts to its agents.",
        "Role: orchestrator",
        "Role: coder",
        "command: claude --model sonnet",
        "description: The coder of the team",
        "prompt: Write the code.",
        "a approve and start · Esc cancel · ↑↓ scroll",
    ] {
        assert!(never.contains(want), "missing {want:?}:\n{never}");
    }
    for unwanted in ["  changed", "  new", "Removed since", "(more", "restore"] {
        assert!(
            !never.contains(unwanted),
            "unexpected {unwanted:?}:\n{never}"
        );
    }
    assert_flags(&never, "orchestrator", true, false);
    assert_flags(&never, "coder", false, false);
    insta::assert_snapshot!("prompt_003_never_approved_start", never);
}

/// Scenario: Approve a three-role orchestration, then change one role's command, add a role and remove another, and draw the prompt the New Agent form shows. It must say `changed since you approved it`, mark the edited role `changed` and the added one `new`, leave the untouched role unmarked, name the removed role under `Removed since you approved it`, and show every listed role's `start` and `clear` values.
#[spec("config-trust/prompt/003")]
#[test]
fn prompt_003_changed_orchestration_marks_roles() {
    let changed = text(
        &view(Some(&approved_team()), &changed_team()),
        PromptAction::Start,
        0,
        100,
        40,
    );
    for want in [
        "State: changed since you approved it",
        "Role: coder  changed",
        "command: claude --model opus",
        "Role: tester  new",
        "Removed since you approved it: reviewer",
        "a approve and start · Esc cancel · ↑↓ scroll",
    ] {
        assert!(changed.contains(want), "missing {want:?}:\n{changed}");
    }
    let orchestrator_row = changed
        .lines()
        .find(|l| l.contains("Role: orchestrator"))
        .expect("the orchestrator role is listed");
    assert!(
        !orchestrator_row.contains("changed") && !orchestrator_row.contains("new"),
        "the unchanged role is not marked: {orchestrator_row:?}"
    );
    assert!(
        !changed.contains("Role: reviewer"),
        "a removed role is not listed as a role:\n{changed}"
    );
    assert_flags(&changed, "orchestrator", true, false);
    assert_flags(&changed, "coder", false, false);
    assert_flags(&changed, "tester", false, false);
    insta::assert_snapshot!("prompt_003_changed_start", changed);
}

/// Scenario: Approve a three-role orchestration, then change nothing but flags: first the `coder` worker's `clear` from false to true, then the `start` role moved from `orchestrator` to `coder`. Each time the prompt must mark exactly the roles whose flag moved as `changed` and show every role's `start` and `clear` values, so the user can see the flag they are approving.
#[spec("config-trust/prompt/003")]
#[test]
fn prompt_003_flag_only_change_shows_start_and_clear() {
    // 1. `clear` false -> true on an approved worker, nothing else.
    let mut cleared = approved_team();
    cleared.roles[1].clear = true;
    let screen = text(
        &view(Some(&approved_team()), &cleared),
        PromptAction::Start,
        0,
        100,
        50,
    );
    assert!(
        screen.contains("State: changed since you approved it"),
        "{screen}"
    );
    assert!(screen.contains("Role: coder  changed"), "{screen}");
    for unmarked in ["orchestrator", "reviewer"] {
        let head = role_block(&screen, unmarked);
        let head = head.lines().next().unwrap_or_default();
        assert!(
            !head.contains("changed") && !head.contains("new"),
            "role {unmarked:?} did not change and is not marked: {head:?}"
        );
    }
    assert_flags(&screen, "coder", false, true);
    assert_flags(&screen, "orchestrator", true, false);
    assert_flags(&screen, "reviewer", false, false);

    // 2. The start role moves from `orchestrator` to `coder`.
    let mut moved = approved_team();
    moved.roles[0].start = false;
    moved.roles[1].start = true;
    let screen = text(
        &view(Some(&approved_team()), &moved),
        PromptAction::Start,
        0,
        100,
        50,
    );
    assert!(screen.contains("Role: orchestrator  changed"), "{screen}");
    assert!(screen.contains("Role: coder  changed"), "{screen}");
    assert_flags(&screen, "orchestrator", false, false);
    assert_flags(&screen, "coder", true, false);
    assert_flags(&screen, "reviewer", false, false);
}

/// Scenario: Draw the same changed orchestration as session restore asks about it. The footer must offer `a approve and restore` instead of `a approve and start`, with the same role marks.
#[spec("config-trust/prompt/003")]
#[test]
fn prompt_003_restore_variant_offers_restore() {
    let restore = text(
        &view(Some(&approved_team()), &changed_team()),
        PromptAction::Restore,
        0,
        100,
        40,
    );
    assert!(
        restore.contains("a approve and restore · Esc cancel · ↑↓ scroll"),
        "{restore}"
    );
    assert!(!restore.contains("approve and start"), "{restore}");
    assert!(restore.contains("Role: coder  changed"), "{restore}");
    assert!(restore.contains("Role: tester  new"), "{restore}");
    insta::assert_snapshot!("prompt_003_changed_restore", restore);
}

/// Scenario: Draw a never-approved orchestration whose role prompt runs to forty lines in a short terminal, at the top, scrolled to the middle and scrolled past the end. The footer must stay on screen every time, showing `(more below)` at the top, `(more above and below)` in the middle and `(more above)` at the end, where the prompt's last line is visible.
#[spec("config-trust/prompt/003")]
#[test]
fn prompt_003_long_prompt_scrolls_under_fixed_footer() {
    let long_prompt = (1..=40)
        .map(|n| format!("Step {n:02}: do the next thing carefully."))
        .collect::<Vec<_>>()
        .join("\n");
    let team = orchestration(vec![role("orchestrator", "claude", &long_prompt)]);
    let v = view(None, &team);
    let (w, h) = (80, 20);

    let top = text(&v, PromptAction::Start, 0, w, h);
    assert!(top.contains("State: not approved before"), "{top}");
    assert!(top.contains("(more below)"), "{top}");
    assert!(!top.contains("(more above"), "{top}");
    assert!(!top.contains("Step 40"), "{top}");

    let middle = text(&v, PromptAction::Start, 15, w, h);
    assert!(middle.contains("(more above and below)"), "{middle}");
    assert!(
        middle.contains("a approve and start · Esc cancel · ↑↓ scroll"),
        "the footer stays put while the body scrolls:\n{middle}"
    );
    assert!(
        !middle.contains("State: not approved before"),
        "the header scrolled away:\n{middle}"
    );

    // Past the end clamps to the last page.
    let end = text(&v, PromptAction::Start, u16::MAX, w, h);
    assert!(end.contains("(more above)"), "{end}");
    assert!(!end.contains("below)"), "{end}");
    assert!(
        end.contains("Step 40: do the next thing carefully."),
        "{end}"
    );
    assert!(
        end.contains("a approve and start · Esc cancel · ↑↓ scroll"),
        "{end}"
    );

    insta::assert_snapshot!("prompt_003_long_scrolled_middle", middle);
    insta::assert_snapshot!("prompt_003_long_scrolled_end", end);
}

/// Scenario: Draw a never-approved orchestration whose first role's prompt runs to 70,000 short lines (more rows than a terminal widget can scroll), followed by a second role with a distinctive command, scrolled as far down as the prompt goes. Either that second role's command is on screen there, or the prompt does not offer `a approve` at all: approval is never offered for a command the user cannot scroll to.
#[spec("config-trust/prompt/003")]
#[test]
fn prompt_003_a_prompt_beyond_65535_rows_cannot_hide_a_later_role() {
    const LATER_COMMAND: &str = "LATER-ROLE-COMMAND-7f3a --run-this";
    let huge_prompt = vec!["x"; 70_000].join("\n");
    let team = orchestration(vec![
        role("orchestrator", "claude", &huge_prompt),
        role("coder", LATER_COMMAND, "Write the code."),
    ]);
    let end = text(&view(None, &team), PromptAction::Start, u16::MAX, 100, 40);
    let offers_approval = end.contains("a approve");
    assert!(
        end.contains(LATER_COMMAND) || !offers_approval,
        "scrolled to the end, the prompt still offers approval but the later role's command \
         {LATER_COMMAND:?} is unreachable — `a` would approve a command the user cannot see.\n\
         Last page:\n{end}"
    );
}
