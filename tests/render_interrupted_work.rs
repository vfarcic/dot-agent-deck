//! PRD #1790 stage 2: the TUI's interrupted work — the footer row that counts
//! what a deck restart left behind, the "Interrupted work" list it opens,
//! an item's details, the reclaim and dismiss-all confirmations, and the
//! restart notice's line leading to the list. L1: in-process `TestBackend`
//! and `insta`, no PTY; the daemon's answers are handed to the TUI through
//! the seam's steps, exactly where the background request would deliver them.
//!
//! Every sentence asserted is one of the shared wording constants of
//! `dot_agent_deck::interrupted_work` (decision D2), so the CLI, the TUI and
//! the desktop say the same thing.
//!
//! Keys: `I` opens the list from the dashboard (the footer's `[Show I]`, and
//! the restart notice's list), `X` hides the footer row; in the list `j`/`k`
//! select, `Enter` opens details (and there shows the kept reports' text),
//! `r` reclaims, `d` dismisses, `D` dismisses all, `Esc`/`q` close.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::interrupted_work::{
    DISMISSED, DismissOutcome, DismissedItem, INTERRUPTED_WORK_TITLE, InterruptedItem,
    InterruptedList, InterruptedShow, ItemKind, ItemReport, LABEL_BRANCH, LABEL_FOLDER,
    LABEL_LAST_ENTRY, LABEL_PROGRESS, LABEL_REPORT, LABEL_STARTED_BY, LABEL_WORKTREE, LastEntry,
    OTHER_AGENTS_GROUP, Progress, RECLAIM_EXPLANATION, RECLAIM_REMOVED, RECLAIM_WOULD_REMOVE,
    ReclaimOutcome, ReclaimVerdict, ShownReport, dismiss_all_question, interrupted_footer,
    kept_until, kind_label, progress_line, unpushed_sentence,
};
use dot_agent_deck::ui::{InterruptedSeamStep, UiMode, render_interrupted_work_to_buffers};
use dot_agent_deck::work_journal::WorkId;
use spec::spec;

const WIDTH: u16 = 110;
const HEIGHT: u16 = 32;
/// 2026-11-19 10:00 UTC, the last journal line of every fixture item.
const LAST_TS: u64 = 1_795_082_400_000;
/// 2026-11-26 10:00 UTC, the kept report's removal date (D7: 7 days on).
const KEPT_TS: u64 = 1_795_687_200_000;
const UNIT_ID: &str = "w-abcdefgh-0000000000000001";
const TEAM_ID: &str = "w-abcdefgh-0000000000000002";
const AGENT_ID: &str = "w-abcdefgh-0000000000000003";
const REPORT_ID: &str = "r-fix-login-done";

fn text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn key(code: KeyCode) -> InterruptedSeamStep {
    InterruptedSeamStep::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(c: char) -> InterruptedSeamStep {
    key(KeyCode::Char(c))
}

fn item(id: &str, kind: ItemKind, name: &str) -> InterruptedItem {
    InterruptedItem {
        id: WorkId::parse(id).expect("a fixture work id"),
        kind,
        name: Some(name.to_string()),
        issue: None,
        repo: None,
        branch: None,
        worktree: None,
        launcher: None,
        last_entry: LastEntry {
            kind: "attempt_ended".to_string(),
            ts_ms: LAST_TS,
        },
        reports: Vec::new(),
        reports_omitted: 0,
        cut: false,
        progress: Progress {
            attempts: 1,
            ..Progress::default()
        },
        git: None,
    }
}

/// A dispatched unit with a kept report.
fn unit() -> InterruptedItem {
    let mut unit = item(UNIT_ID, ItemKind::Unit, "fix-login");
    unit.branch = Some("agent/dispatch-fix-login".to_string());
    unit.worktree = Some("/home/me/code/app-dispatch-fix-login".to_string());
    unit.launcher = Some("dispatcher".to_string());
    unit.progress.roles = vec!["coder".to_string()];
    unit.reports = vec![ItemReport {
        report_id: REPORT_ID.to_string(),
        task_id: None,
        unit_id: None,
        kept_until_ms: Some(KEPT_TS),
    }];
    unit
}

/// An orchestration started from a client.
fn team() -> InterruptedItem {
    let mut team = item(TEAM_ID, ItemKind::Orchestration, "release-team");
    team.worktree = Some("/home/me/code/app".to_string());
    team.progress = Progress {
        attempts: 3,
        roles: vec![
            "orchestrator".to_string(),
            "coder".to_string(),
            "reviewer".to_string(),
        ],
        delegations: 4,
        delegations_done: 2,
    };
    team
}

/// A plain agent: listed under "Other agents", dismiss only (D1).
fn agent() -> InterruptedItem {
    let mut agent = item(AGENT_ID, ItemKind::Agent, "scratch-shell");
    agent.worktree = Some("/home/me/notes".to_string());
    agent
}

/// Most recent first, as the daemon answers: the plain agent first, so the
/// grouping (work before "Other agents") is the TUI's doing.
fn three() -> InterruptedList {
    InterruptedList {
        items: vec![agent(), unit(), team()],
        incomplete: false,
        omitted: 0,
    }
}

fn only(item: InterruptedItem) -> InterruptedList {
    InterruptedList {
        items: vec![item],
        incomplete: false,
        omitted: 0,
    }
}

fn frames(list: &InterruptedList, steps: &[InterruptedSeamStep]) -> Vec<String> {
    render_interrupted_work_to_buffers(list, &[], UiMode::Normal, steps, WIDTH, HEIGHT)
        .iter()
        .map(text)
        .collect()
}

fn at(shown: &str, needle: &str) -> usize {
    shown
        .find(needle)
        .unwrap_or_else(|| panic!("the frame shows {needle:?}:\n{shown}"))
}

/// Scenario: A deck restart left one interrupted unit. On the dashboard the
/// footer shows "1 interrupted item from before the restart" with a `[Show I]`
/// and a `[Hide X]` button; with three items it says "3 interrupted items";
/// with none there is no row. Pressing `X` hides the row.
#[spec("journal/interrupted/008")]
#[test]
fn interrupted_008_the_footer_counts_the_interrupted_items() {
    let one = frames(&only(unit()), &[ch('X')]);
    assert!(one[0].contains(&interrupted_footer(1)), "{}", one[0]);
    assert!(
        one[0].contains("1 interrupted item from before the restart"),
        "singular: {}",
        one[0]
    );
    assert!(one[0].contains("[Show I]"), "{}", one[0]);
    assert!(one[0].contains("[Hide X]"), "{}", one[0]);
    insta::assert_snapshot!("footer_one_item", one[0]);
    assert!(
        !one[1].contains("interrupted item"),
        "`X` hides the footer row:\n{}",
        one[1]
    );

    let many = frames(&three(), &[]);
    assert!(
        many[0].contains("3 interrupted items from before the restart"),
        "plural: {}",
        many[0]
    );
    insta::assert_snapshot!("footer_three_items", many[0]);

    let none = frames(&InterruptedList::default(), &[]);
    assert!(
        !none[0].contains("interrupted item") && !none[0].contains("[Show I]"),
        "no items, no footer row:\n{}",
        none[0]
    );
}

/// Scenario: With a unit, an orchestration and a plain agent left by a
/// restart, pressing `I` opens the "Interrupted work" list: the unit and the
/// orchestration first, each with its name, kind, branch, worktree, launcher
/// and last entry, then the plain agent under "Other agents". The selected
/// row shows its progress line; `j` moves the selection and the progress
/// line with it; `r` on the plain agent offers no reclaim; `Esc` closes.
#[spec("journal/interrupted/009")]
#[test]
fn interrupted_009_the_list_groups_work_before_other_agents() {
    let shown = frames(
        &three(),
        &[ch('I'), ch('j'), ch('j'), ch('r'), key(KeyCode::Esc)],
    );
    let open = &shown[1];
    insta::assert_snapshot!("list_three_items", open);
    let title = at(open, INTERRUPTED_WORK_TITLE);
    let unit_at = at(open, "fix-login");
    let team_at = at(open, "release-team");
    let group = at(open, OTHER_AGENTS_GROUP);
    let agent_at = at(open, "scratch-shell");
    assert!(
        title < unit_at && unit_at < group && team_at < group && group < agent_at,
        "units and orchestrations first, plain agents under {OTHER_AGENTS_GROUP:?}:\n{open}"
    );
    for needle in [
        kind_label(ItemKind::Unit),
        kind_label(ItemKind::Orchestration),
        kind_label(ItemKind::Agent),
        "agent/dispatch-fix-login",
        "/home/me/code/app-dispatch-fix-login",
        "dispatcher",
        "attempt_ended",
    ] {
        at(open, needle);
    }
    let unit_progress = format!("{LABEL_PROGRESS}: {}", progress_line(&unit().progress));
    let team_progress = format!("{LABEL_PROGRESS}: {}", progress_line(&team().progress));
    assert!(
        open.contains(&unit_progress) && !open.contains(&team_progress),
        "only the selected row (the first, the unit) shows its progress:\n{open}"
    );
    let moved = &shown[2];
    assert!(
        moved.contains(&team_progress) && !moved.contains(&unit_progress),
        "`j` selects the orchestration:\n{moved}"
    );
    let on_agent = &shown[3];
    assert!(
        on_agent.contains(&format!(
            "{LABEL_PROGRESS}: {}",
            progress_line(&agent().progress)
        )),
        "a second `j` selects the plain agent:\n{on_agent}"
    );
    assert!(
        !on_agent.contains("r reclaim"),
        "a plain agent offers no reclaim (D1):\n{on_agent}"
    );
    assert_eq!(shown[4], shown[3], "`r` on a plain agent does nothing (D1)");
    assert!(
        !shown[5].contains(INTERRUPTED_WORK_TITLE),
        "`Esc` closes the list:\n{}",
        shown[5]
    );
}

/// Scenario: In the "Interrupted work" list, `Enter` on the unit opens its
/// details: its kind, branch, worktree, launcher, last entry, progress and
/// its kept report with "kept until <date>", but not the report's text.
/// `Enter` again asks for the text; when the daemon answers, it is shown as
/// the framed untrusted text the daemon sent. `Esc` goes back to the list.
#[spec("journal/interrupted/010")]
#[test]
fn interrupted_010_details_show_every_field_and_kept_reports() {
    let body = "[UNTRUSTED-WORKER-REPORT: written by an agent]\nLogin fixed; tests pass.\n:END-UNTRUSTED-WORKER-REPORT]\n";
    let show = InterruptedShow {
        item: unit(),
        reports: vec![ShownReport {
            report_id: REPORT_ID.to_string(),
            body: Some(body.to_string()),
            cut: false,
            error: None,
        }],
        incomplete: false,
        bodies_omitted: 0,
    };
    let shown = frames(
        &only(unit()),
        &[
            ch('I'),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            InterruptedSeamStep::Shown(show),
            key(KeyCode::Esc),
        ],
    );
    let details = &shown[2];
    insta::assert_snapshot!("details_with_kept_report", details);
    for needle in [
        format!("{LABEL_BRANCH}: agent/dispatch-fix-login"),
        format!("{LABEL_WORKTREE}: /home/me/code/app-dispatch-fix-login"),
        format!("{LABEL_STARTED_BY}: dispatcher"),
        format!("{LABEL_LAST_ENTRY}: attempt_ended"),
        format!("{LABEL_PROGRESS}: {}", progress_line(&unit().progress)),
        format!("{LABEL_REPORT}: {REPORT_ID}, {}", kept_until(Some(KEPT_TS))),
    ] {
        at(details, &needle);
    }
    assert!(
        details.contains("kept until 2026-11-26 10:00 UTC"),
        "D7: {details}"
    );
    assert!(
        !details.contains("Login fixed"),
        "a report's text is shown only on demand:\n{details}"
    );
    let with_body = &shown[4];
    assert!(
        with_body.contains("[UNTRUSTED-WORKER-REPORT:")
            && with_body.contains("Login fixed; tests pass.")
            && with_body.contains(":END-UNTRUSTED-WORKER-REPORT]"),
        "the report's text, framed as untrusted:\n{with_body}"
    );
    let back = &shown[5];
    assert!(
        back.contains(INTERRUPTED_WORK_TITLE) && !back.contains("Login fixed"),
        "`Esc` goes back to the list:\n{back}"
    );

    let agent_details = frames(&only(agent()), &[ch('I'), key(KeyCode::Enter)]);
    assert!(
        agent_details[2].contains(&format!("{LABEL_FOLDER}: /home/me/notes")),
        "a plain agent's directory is its folder, as the CLI says:\n{}",
        agent_details[2]
    );
}

/// Scenario: In the "Interrupted work" list, `r` on the unit asks the
/// daemon for a dry run first; when it answers, the confirmation shows the
/// dry-run verdict, what reclaim means ("remove its worktree, keep its
/// branch") and the commits on no remote, and asks y/n. `y` runs the real
/// reclaim, and its sentence is shown when the daemon answers; the list then
/// refreshes from the daemon and, emptied, the footer disappears.
#[spec("journal/interrupted/011")]
#[test]
fn interrupted_011_reclaim_confirms_with_the_dry_run_verdict() {
    let dry = ReclaimOutcome {
        id: Some(UNIT_ID.to_string()),
        dry_run: true,
        verdict: ReclaimVerdict::WouldRemove,
        message: RECLAIM_WOULD_REMOVE.to_string(),
        refusal: None,
        worktree: unit().worktree,
        branch: unit().branch,
        unpushed_commits: Some(2),
        nested_worktrees: Vec::new(),
        recorded: false,
    };
    let done = ReclaimOutcome {
        dry_run: false,
        verdict: ReclaimVerdict::Removed,
        message: RECLAIM_REMOVED.to_string(),
        recorded: true,
        ..dry.clone()
    };
    let shown = frames(
        &only(unit()),
        &[
            ch('I'),
            ch('r'),
            InterruptedSeamStep::Reclaimed(dry),
            ch('y'),
            InterruptedSeamStep::Reclaimed(done),
            InterruptedSeamStep::Listed(InterruptedList::default()),
        ],
    );
    let confirm = &shown[3];
    insta::assert_snapshot!("reclaim_confirm_dry_run", confirm);
    for needle in [
        RECLAIM_WOULD_REMOVE.to_string(),
        RECLAIM_EXPLANATION.to_string(),
        unpushed_sentence(2).unwrap(),
        "agent/dispatch-fix-login".to_string(),
    ] {
        at(confirm, &needle);
    }
    assert!(confirm.contains("y/n"), "it asks y/n:\n{confirm}");
    let reclaimed = &shown[5];
    assert!(
        reclaimed.contains(RECLAIM_REMOVED),
        "the real reclaim's sentence:\n{reclaimed}"
    );
    assert!(
        reclaimed.contains(&interrupted_footer(1)),
        "until the refreshed list arrives the footer still counts it:\n{reclaimed}"
    );
    let refreshed = &shown[6];
    assert!(
        !refreshed.contains(&interrupted_footer(1)) && !refreshed.contains("[Show I]"),
        "an emptied list leaves no footer:\n{refreshed}"
    );
}

/// Scenario: In the "Interrupted work" list of three items, `D` asks "Dismiss
/// all: 3 interrupted items from before the restart?"; `n` goes back with
/// nothing dismissed. `d` dismisses the selected item: the daemon's answer
/// shows "Dismissed." and the refreshed list no longer has it.
#[spec("journal/interrupted/012")]
#[test]
fn interrupted_012_dismiss_and_dismiss_all_confirm() {
    let shown = frames(&three(), &[ch('I'), ch('D'), ch('n')]);
    let confirm = &shown[2];
    insta::assert_snapshot!("dismiss_all_confirm", confirm);
    at(confirm, dismiss_all_question(3).trim_end());
    assert!(
        shown[3].contains(INTERRUPTED_WORK_TITLE) && !shown[3].contains("Dismiss all:"),
        "`n` goes back to the list:\n{}",
        shown[3]
    );

    let dismissed = DismissOutcome {
        dismissed: vec![DismissedItem {
            id: UNIT_ID.to_string(),
            name: Some("fix-login".to_string()),
        }],
        ..DismissOutcome::default()
    };
    let after = InterruptedList {
        items: vec![agent(), team()],
        incomplete: false,
        omitted: 0,
    };
    let shown = frames(
        &three(),
        &[
            ch('I'),
            ch('d'),
            InterruptedSeamStep::Dismissed(dismissed),
            InterruptedSeamStep::Listed(after),
        ],
    );
    assert!(shown[3].contains(DISMISSED), "{}", shown[3]);
    let refreshed = &shown[4];
    assert!(
        !refreshed.contains("fix-login") && refreshed.contains("release-team"),
        "the dismissed unit leaves the refreshed list:\n{refreshed}"
    );
    assert!(refreshed.contains(&interrupted_footer(2)), "{refreshed}");
}

/// Scenario: A deck restart stopped two agents and left three interrupted
/// items. The restart notice's list (`a`) keeps its words and gains a line
/// saying how many interrupted items there are and that `I` shows them;
/// pressing `I` there opens the "Interrupted work" list.
#[spec("journal/interrupted/013")]
#[test]
fn interrupted_013_the_restart_notice_leads_to_the_list() {
    let shown: Vec<String> = render_interrupted_work_to_buffers(
        &three(),
        &["coder", "reviewer"],
        UiMode::RestartNotice,
        &[ch('I')],
        WIDTH,
        HEIGHT,
    )
    .iter()
    .map(text)
    .collect();
    let notice = &shown[0];
    insta::assert_snapshot!("restart_notice_hint", notice);
    at(notice, "The deck restarted; these agents were stopped:");
    let hint = notice
        .lines()
        .find(|line| line.contains(&interrupted_footer(3)) && line.contains('│'))
        .unwrap_or_else(|| panic!("the notice's list names the interrupted items:\n{notice}"));
    assert!(hint.contains('I'), "the hint names the key: {hint}");
    assert!(
        shown[1].contains(INTERRUPTED_WORK_TITLE) && shown[1].contains("fix-login"),
        "`I` opens the interrupted work list:\n{}",
        shown[1]
    );
}
