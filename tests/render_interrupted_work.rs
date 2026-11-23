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
//! select, `Enter` opens details (and there shows the kept reports' text,
//! with `n`/`p` for the next and previous page of them),
//! `r` reclaims, `d` dismisses, `D` dismisses all, `Esc`/`q` close.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::interrupted_work::{
    DISMISSED, DismissOutcome, DismissStopReason, DismissStopped, DismissedItem, GitFacts,
    INTERRUPTED_UNREAD, INTERRUPTED_WORK_TITLE, InterruptedItem, InterruptedList, InterruptedShow,
    ItemKind, ItemReport, LABEL_BRANCH, LABEL_FOLDER, LABEL_GIT, LABEL_LAST_ENTRY, LABEL_PROGRESS,
    LABEL_REPORT, LABEL_STARTED_BY, LABEL_WORKTREE, LIST_INCOMPLETE, LIST_REFRESH_FAILED,
    LastEntry, OTHER_AGENTS_GROUP, PrFact, Progress, RECLAIM_EXPLANATION, RECLAIM_REMOVED,
    RECLAIM_WOULD_REMOVE, ReclaimOutcome, ReclaimVerdict, Refusal, RefusalReason, ShownReport,
    UNSUPPORTED, dismiss_all_question, dismiss_stopped_sentence, git_facts_line,
    interrupted_footer, kept_until, kind_label, progress_line, refusal_sentence, unpushed_sentence,
};
use dot_agent_deck::keybindings::KeybindingConfig;
use dot_agent_deck::ui::{
    INTERRUPTED_SEAM_FIRST_DAEMON, InterruptedSeamKind, InterruptedSeamRequest,
    InterruptedSeamStep, UiMode, render_interrupted_work_to_buffers, run_interrupted_work_seam,
};
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
        agents: Vec::new(),
        agents_omitted: 0,
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
        reports_from: 0,
        next_reports_from: None,
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

fn report(n: usize) -> ItemReport {
    ItemReport {
        report_id: format!("r-{n}"),
        task_id: None,
        unit_id: None,
        kept_until_ms: Some(KEPT_TS),
    }
}

/// The unit holding reports `first..=last` of `total`, as a show page that
/// starts at `first - 1`, with the body of `body_of` only.
fn page(first: usize, last: usize, total: usize, body_of: Option<usize>) -> InterruptedShow {
    let mut item = unit();
    item.reports = (first..=last).map(report).collect();
    item.reports_omitted = total - item.reports.len();
    InterruptedShow {
        item,
        reports: body_of
            .map(|n| ShownReport {
                report_id: format!("r-{n}"),
                body: Some(format!("[UNTRUSTED-WORKER-REPORT:\nBODY OF r-{n}\n")),
                cut: false,
                error: None,
            })
            .into_iter()
            .collect(),
        incomplete: false,
        bodies_omitted: 0,
        reports_from: first - 1,
        next_reports_from: (last < total).then_some(last),
    }
}

/// Scenario: A unit kept eight reports, more than its list entry carries
/// (three, and "5 more reports are kept"). In its details `Enter` shows the
/// first page of reports, "Reports 1 to 3 of 8.", and offers `n` for the
/// next ones; `n` shows "Reports 4 to 6 of 8." with a report's text; a page
/// the TUI did not ask for is not shown; `n` again reaches the last page,
/// which offers no next one, and `p` goes back to the first.
#[spec("journal/interrupted/016")]
#[test]
fn interrupted_016_details_page_through_every_kept_report() {
    let mut listed = unit();
    listed.reports = (1..=3).map(report).collect();
    listed.reports_omitted = 5;
    let shown = frames(
        &only(listed),
        &[
            ch('I'),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            InterruptedSeamStep::Shown(page(1, 3, 8, Some(1))),
            ch('n'),
            InterruptedSeamStep::Shown(page(4, 6, 8, Some(5))),
            InterruptedSeamStep::Shown(page(7, 8, 8, None)),
            ch('n'),
            InterruptedSeamStep::Shown(page(7, 8, 8, None)),
            ch('p'),
            InterruptedSeamStep::Shown(page(1, 3, 8, Some(1))),
        ],
    );
    let details = &shown[2];
    assert!(
        details.contains("5 more reports are kept"),
        "the details count the reports the list left out:\n{details}"
    );
    let first = &shown[4];
    at(first, "Reports 1 to 3 of 8.");
    at(first, "n next reports");
    assert!(!first.contains("p previous reports"), "{first}");
    let second = &shown[6];
    at(second, "Reports 4 to 6 of 8.");
    at(second, &format!("{LABEL_REPORT}: r-4"));
    at(second, "BODY OF r-5");
    at(second, "p previous reports");
    let unasked = &shown[7];
    assert!(
        unasked.contains("Reports 4 to 6 of 8.") && !unasked.contains("Reports 7 to 8 of 8."),
        "a page the TUI did not ask for is not shown:\n{unasked}"
    );
    let last = &shown[9];
    at(last, "Reports 7 to 8 of 8.");
    at(last, &format!("{LABEL_REPORT}: r-8"));
    assert!(!last.contains("n next reports"), "the last page:\n{last}");
    let back = &shown[11];
    at(back, "Reports 1 to 3 of 8.");
}

/// Scenario: A unit's name, branch, worktree and launcher, and a line of
/// its report's text, carry Unicode bidi controls (a right-to-left override
/// a Git branch name may hold). Neither the "Interrupted work" list nor the
/// unit's details draw one, so nothing on screen is reordered; the text
/// around them is still shown, and the details show a control escaped
/// (`\u{202e}`), as the CLI does.
#[spec("journal/interrupted/017")]
#[test]
fn interrupted_017_bidi_controls_from_the_journal_are_not_drawn() {
    let bidi = |c: char| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}');
    let mut evil = unit();
    evil.name = Some("fix\u{202e}nigol".to_string());
    evil.branch = Some("agent/\u{202e}txt.exe".to_string());
    evil.worktree = Some("/home/me/\u{2066}code\u{2069}".to_string());
    evil.launcher = Some("dispatcher\u{200f}".to_string());
    let mut show = page(1, 1, 1, None);
    show.item = evil.clone();
    show.reports = vec![ShownReport {
        report_id: REPORT_ID.to_string(),
        body: Some("[UNTRUSTED-WORKER-REPORT:\nall \u{202e}done\n".to_string()),
        cut: false,
        error: None,
    }];
    let shown = frames(
        &only(evil),
        &[
            ch('I'),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            InterruptedSeamStep::Shown(show),
        ],
    );
    for (at_step, frame) in shown.iter().enumerate() {
        assert!(
            !frame.chars().any(bidi),
            "frame {at_step} draws a bidi control:\n{frame:?}"
        );
    }
    assert!(shown[1].contains("nigol"), "{}", shown[1]);
    assert!(shown[4].contains("done"), "{}", shown[4]);
    // Shown escaped, as the CLI shows it, so the person sees it is there.
    at(
        &shown[2],
        &format!("{LABEL_BRANCH}: agent/\\u{{202e}}txt.exe"),
    );
}

// ---------------------------------------------------------------------------
// The TUI review's findings (#1790 stage 2): what the TUI sends, and which
// answers it applies. `run` records every request instead of sending it.
// ---------------------------------------------------------------------------

const NEW_ID: &str = "w-abcdefgh-0000000000000004";
const SECOND_DAEMON: &str = "seam-daemon-2";

/// The frames drawn and the requests sent by `steps`, with `keybindings`, at
/// `width` × `height`.
fn run_with(
    list: &InterruptedList,
    keybindings: &KeybindingConfig,
    steps: &[InterruptedSeamStep],
    width: u16,
    height: u16,
) -> (Vec<String>, Vec<InterruptedSeamRequest>) {
    let run =
        run_interrupted_work_seam(list, &[], UiMode::Normal, keybindings, steps, width, height);
    (
        run.frames.iter().map(text).collect(),
        run.sent.into_iter().map(|sent| sent.request).collect(),
    )
}

fn run(
    list: &InterruptedList,
    steps: &[InterruptedSeamStep],
) -> (Vec<String>, Vec<InterruptedSeamRequest>) {
    run_with(list, &KeybindingConfig::default(), steps, WIDTH, HEIGHT)
}

fn dismissal(ids: &[&str]) -> InterruptedSeamRequest {
    InterruptedSeamRequest::Dismiss {
        ids: ids.iter().map(|id| id.to_string()).collect(),
        all: false,
    }
}

fn dry_run(id: &str) -> InterruptedSeamRequest {
    InterruptedSeamRequest::Reclaim {
        id: id.to_string(),
        dry_run: true,
    }
}

fn real_reclaim(id: &str) -> InterruptedSeamRequest {
    InterruptedSeamRequest::Reclaim {
        id: id.to_string(),
        dry_run: false,
    }
}

fn is_dismissal(request: &InterruptedSeamRequest) -> bool {
    matches!(request, InterruptedSeamRequest::Dismiss { .. })
}

fn is_real_reclaim(request: &InterruptedSeamRequest) -> bool {
    matches!(
        request,
        InterruptedSeamRequest::Reclaim { dry_run: false, .. }
    )
}

fn list_of(items: Vec<InterruptedItem>) -> InterruptedList {
    InterruptedList {
        items,
        incomplete: false,
        omitted: 0,
    }
}

/// The unit's dry run, which would remove its worktree.
fn would_remove() -> ReclaimOutcome {
    ReclaimOutcome {
        id: Some(UNIT_ID.to_string()),
        dry_run: true,
        verdict: ReclaimVerdict::WouldRemove,
        message: RECLAIM_WOULD_REMOVE.to_string(),
        refusal: None,
        worktree: unit().worktree,
        branch: unit().branch,
        unpushed_commits: None,
        nested_worktrees: Vec::new(),
        recorded: false,
    }
}

fn removed() -> ReclaimOutcome {
    ReclaimOutcome {
        dry_run: false,
        verdict: ReclaimVerdict::Removed,
        message: RECLAIM_REMOVED.to_string(),
        recorded: true,
        ..would_remove()
    }
}

/// Every character a journal value must not put on screen as itself: the C0
/// and C1 controls, DEL, and the bidi formatting characters.
fn hostile(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
        )
}

fn every_hostile_char() -> String {
    (0u32..=0x9f)
        .filter_map(char::from_u32)
        .filter(|c| c.is_control())
        .chain(
            ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}']
                .into_iter()
                .chain(['\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'])
                .chain(['\u{200E}', '\u{200F}', '\u{061C}']),
        )
        .collect()
}

/// The overlay's box in `buffer`: every row between its top and bottom
/// border starts and ends with a vertical border cell.
fn assert_box_intact(buffer: &ratatui::buffer::Buffer, at_step: usize) {
    let area = buffer.area();
    let find = |want: &str| {
        (0..area.height).find_map(|y| {
            (0..area.width).find_map(|x| (buffer[(x, y)].symbol() == want).then_some((x, y)))
        })
    };
    let Some((left, top)) = find("┌") else {
        return;
    };
    let right = (left..area.width)
        .find(|x| buffer[(*x, top)].symbol() == "┐")
        .unwrap_or_else(|| panic!("frame {at_step}: the box has a top right corner"));
    let bottom = (top..area.height)
        .find(|y| buffer[(left, *y)].symbol() == "└")
        .unwrap_or_else(|| panic!("frame {at_step}: the box has a bottom left corner"));
    assert_eq!(buffer[(right, bottom)].symbol(), "┘", "frame {at_step}");
    for y in top + 1..bottom {
        assert!(
            buffer[(left, y)].symbol() == "│" && buffer[(right, y)].symbol() == "│",
            "frame {at_step}: row {y} keeps both borders:\n{}",
            text(buffer)
        );
    }
}

/// Scenario: Every journal value the TUI shows carries C0 and C1 controls
/// and every bidi formatting character, around long Chinese paths: an item's
/// name, branch, worktree, launcher, last entry, roles and repository, its
/// report's id and text, a reclaim dry run's paths and an unknown verdict's
/// sentence, and a failed request's message. No frame of the list, details,
/// shown reports or reclaim draws any of them, and the overlay's borders stay
/// whole. A short report shown on screen draws each such character as its
/// escaped spelling.
#[spec("journal/interrupted/018")]
#[test]
fn interrupted_018_hostile_journal_values_are_escaped_everywhere_they_are_shown() {
    let evil = every_hostile_char();
    let long = format!(
        "/home/me/代码/{}{evil}/尾巴",
        "非常长的目录名称/".repeat(12)
    );
    let mut item = unit();
    item.name = Some(format!("fix{evil}login"));
    item.branch = Some(format!("agent/{evil}分支-{}", "长".repeat(60)));
    item.worktree = Some(long.clone());
    item.launcher = Some(format!("dispatcher{evil}"));
    item.last_entry.kind = format!("attempt{evil}_ended");
    item.progress.roles = vec![format!("coder{evil}")];
    item.repo = Some(format!("me/app{evil}"));
    item.issue = Some(7);
    item.reports[0].report_id = format!("r-{evil}");
    let mut show = page(1, 1, 1, None);
    show.item = item.clone();
    show.reports = vec![ShownReport {
        report_id: format!("r-{evil}"),
        body: Some(format!("[UNTRUSTED-WORKER-REPORT:\nall {evil} done\n")),
        cut: false,
        error: None,
    }];
    let dry = ReclaimOutcome {
        worktree: Some(long.clone()),
        branch: item.branch.clone(),
        nested_worktrees: vec![format!("{long}/嵌套{evil}")],
        ..would_remove()
    };
    let unknown = ReclaimOutcome {
        verdict: ReclaimVerdict::Unknown,
        message: format!("a newer daemon says {evil} this"),
        ..dry.clone()
    };
    let steps = [
        ch('I'),
        key(KeyCode::Enter),
        key(KeyCode::Enter),
        InterruptedSeamStep::Shown(show),
        key(KeyCode::Esc),
        ch('r'),
        InterruptedSeamStep::Reclaimed(dry),
        key(KeyCode::Esc),
        ch('r'),
        InterruptedSeamStep::Reclaimed(unknown),
        key(KeyCode::Esc),
        ch('d'),
        InterruptedSeamStep::Failed(
            InterruptedSeamKind::Dismiss,
            format!("the daemon failed {evil} here"),
        ),
    ];
    for (width, height) in [(WIDTH, HEIGHT), (80, 24), (40, 24)] {
        let run = run_interrupted_work_seam(
            &only(item.clone()),
            &[],
            UiMode::Normal,
            &KeybindingConfig::default(),
            &steps,
            width,
            height,
        );
        for (at_step, buffer) in run.frames.iter().enumerate() {
            let area = buffer.area();
            for y in 0..area.height {
                for x in 0..area.width {
                    let symbol = buffer[(x, y)].symbol();
                    assert!(
                        !symbol.chars().any(hostile),
                        "{width}x{height} frame {at_step} draws {symbol:?} at ({x}, {y}):\n{}",
                        text(buffer)
                    );
                }
            }
            assert_box_intact(buffer, at_step);
        }
        if width == WIDTH {
            let shown: Vec<String> = run.frames.iter().map(text).collect();
            assert!(shown[1].contains("▶ fix"), "{}", shown[1]);
            assert!(shown[7].contains("y/n"), "{}", shown[7]);
            assert!(shown[10].contains("a newer daemon says"), "{}", shown[10]);
            // Escaped where it is drawn, as the CLI shows it, whatever path
            // the message took.
            assert!(
                shown[13].contains("the daemon failed") && shown[13].contains("\\u{0}\\u{1}"),
                "{}",
                shown[13]
            );
        }
    }

    // The report body on its own, short enough to be drawn below short
    // details: each hostile character on its own line beside a sentinel, so
    // the body is on screen and its escaping, not the widget, keeps each
    // character out. Every one must be drawn as its escaped spelling.
    let mut plain = unit();
    plain.worktree = Some("/w".to_string());
    plain.branch = Some("b".to_string());
    let mut show = page(1, 1, 1, None);
    show.item = plain.clone();
    let probes = ['\u{202e}', '\u{2066}', '\u{1b}', '\u{7}', '\u{85}'];
    show.reports = vec![ShownReport {
        report_id: REPORT_ID.to_string(),
        body: Some(
            probes
                .iter()
                .enumerate()
                .map(|(n, c)| format!("BODY-{n}{c}END\n"))
                .collect(),
        ),
        cut: false,
        error: None,
    }];
    let run = run_interrupted_work_seam(
        &only(plain),
        &[],
        UiMode::Normal,
        &KeybindingConfig::default(),
        &[
            ch('I'),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            InterruptedSeamStep::Shown(show),
        ],
        WIDTH,
        40,
    );
    let shown = text(run.frames.last().expect("a frame"));
    for (n, c) in probes.iter().enumerate() {
        let escaped = if c.is_control() {
            c.escape_default().to_string()
        } else {
            c.escape_unicode().to_string()
        };
        let want = format!("BODY-{n}{escaped}END");
        assert!(
            shown.contains(&want),
            "the report body's line {n} is drawn with {c:?} escaped as {escaped}:\n{shown}"
        );
    }
}

/// Scenario: In the "Interrupted work" list, `r` asks for a dry run and the
/// person backs out with `Esc`, `q` or `n` before it answers. Whether the dry
/// run then answers, fails or never answers, `d` and `r` work again at once,
/// and a late answer of the abandoned dry run is not shown as the answer of
/// a newer one.
#[spec("journal/interrupted/019")]
#[test]
fn interrupted_019_a_cancelled_dry_run_leaves_the_actions_free() {
    for cancel in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('n')] {
        for (what, late) in [
            (
                "answered",
                Some(InterruptedSeamStep::Reclaimed(would_remove())),
            ),
            (
                "failed",
                Some(InterruptedSeamStep::Failed(
                    InterruptedSeamKind::Reclaim,
                    "timed out".to_string(),
                )),
            ),
            ("still out", None),
        ] {
            let mut steps = vec![ch('I'), ch('r'), key(cancel)];
            steps.extend(late);
            steps.push(ch('d'));
            let (shown, sent) = run(&three(), &steps);
            assert_eq!(
                sent.last(),
                Some(&dismissal(&[UNIT_ID])),
                "{cancel:?}, the dry run {what}: `d` dismisses:\n{}",
                shown.last().unwrap()
            );
        }
    }

    // A second dry run while the first, abandoned, is still out.
    let (shown, sent) = run(
        &three(),
        &[
            ch('I'),
            ch('r'),
            key(KeyCode::Esc),
            ch('r'),
            InterruptedSeamStep::AnswerTo(
                2,
                Box::new(InterruptedSeamStep::Reclaimed(would_remove())),
            ),
            InterruptedSeamStep::Reclaimed(would_remove()),
            ch('y'),
        ],
    );
    assert_eq!(
        sent.iter().filter(|r| **r == dry_run(UNIT_ID)).count(),
        2,
        "the second `r` asks again: {sent:?}"
    );
    assert!(
        !shown[5].contains("y/n"),
        "the abandoned dry run's answer is not the newer one's:\n{}",
        shown[5]
    );
    assert!(shown[6].contains("y/n"), "{}", shown[6]);
    assert_eq!(sent.last(), Some(&real_reclaim(UNIT_ID)), "{sent:?}");
}

/// Scenario: The TUI attached to one daemon process asked it for the list,
/// a dry run, a reclaim, a dismissal and a report, and then reconnected to
/// another process before they answered. Each late answer from the first
/// process is dropped — even a list answer whose request number collides
/// with the second's — so the second's list, view and requests stay as they
/// were. The list is fetched once per process the TUI attaches to.
#[spec("journal/interrupted/020")]
#[test]
fn interrupted_020_answers_from_an_earlier_daemon_process_are_dropped() {
    let mut other = item(NEW_ID, ItemKind::Unit, "other-work");
    other.reports = unit().reports;
    let second = only(other.clone());
    let attach = || InterruptedSeamStep::Attach(SECOND_DAEMON.to_string());

    // A list answer, numbered as the second process's newest list request.
    let (shown, sent) = run(
        &three(),
        &[
            ch('I'),
            attach(),
            InterruptedSeamStep::Listed(second.clone()),
            key(KeyCode::Esc),
            ch('I'),
            InterruptedSeamStep::AnswerTo(1, Box::new(InterruptedSeamStep::Listed(three()))),
        ],
    );
    assert_eq!(sent.len(), 4, "{sent:?}");
    let after = shown.last().unwrap();
    assert!(
        after.contains("other-work") && !after.contains("fix-login"),
        "the first process's list does not replace the second's:\n{after}"
    );

    // A dry run, a real reclaim and a dismissal of the first process.
    let cases: Vec<(&str, Vec<InterruptedSeamStep>, usize, InterruptedSeamStep)> = vec![
        (
            "dry run",
            vec![ch('I'), ch('r')],
            2,
            InterruptedSeamStep::Reclaimed(would_remove()),
        ),
        (
            "reclaim",
            vec![
                ch('I'),
                ch('r'),
                InterruptedSeamStep::Reclaimed(would_remove()),
                ch('y'),
            ],
            3,
            InterruptedSeamStep::Reclaimed(removed()),
        ),
        (
            "dismissal",
            vec![ch('I'), ch('d')],
            2,
            InterruptedSeamStep::Dismissed(DismissOutcome {
                dismissed: vec![DismissedItem {
                    id: UNIT_ID.to_string(),
                    name: None,
                }],
                ..DismissOutcome::default()
            }),
        ),
    ];
    for (what, mut steps, index, late) in cases {
        steps.push(attach());
        steps.push(InterruptedSeamStep::Listed(second.clone()));
        let (before, sent_before) = run(&three(), &steps);
        steps.push(InterruptedSeamStep::AnswerTo(index, Box::new(late)));
        let (shown, sent) = run(&three(), &steps);
        assert_eq!(
            sent, sent_before,
            "the first process's {what} answer sends nothing"
        );
        assert_eq!(
            shown.last(),
            before.last(),
            "the first process's {what} answer changes nothing:\n{}",
            shown.last().unwrap()
        );
        // And nothing is left busy: the second process's item can be dismissed.
        steps.push(ch('d'));
        let (_, sent) = run(&three(), &steps);
        assert_eq!(sent.last(), Some(&dismissal(&[NEW_ID])), "after the {what}");
    }

    // A report asked of the first process for an item of the same id.
    let mut same_id = unit();
    same_id.name = Some("second-unit".to_string());
    let mut from_first = page(1, 1, 1, None);
    from_first.reports = vec![ShownReport {
        report_id: REPORT_ID.to_string(),
        body: Some("BODY FROM THE FIRST PROCESS".to_string()),
        cut: false,
        error: None,
    }];
    let (shown, _) = run(
        &three(),
        &[
            ch('I'),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            attach(),
            InterruptedSeamStep::Listed(only(same_id)),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            InterruptedSeamStep::AnswerTo(2, Box::new(InterruptedSeamStep::Shown(from_first))),
        ],
    );
    assert!(
        !shown
            .last()
            .unwrap()
            .contains("BODY FROM THE FIRST PROCESS"),
        "the first process's report is not shown in the second's details:\n{}",
        shown.last().unwrap()
    );

    // One attach fetch per process: none for a frame on the same process.
    let (_, sent) = run(
        &three(),
        &[
            InterruptedSeamStep::Attach(INTERRUPTED_SEAM_FIRST_DAEMON.to_string()),
            attach(),
            attach(),
            InterruptedSeamStep::Listed(second),
            attach(),
        ],
    );
    assert_eq!(
        sent,
        vec![
            InterruptedSeamRequest::List { git: false },
            InterruptedSeamRequest::List { git: false },
        ],
        "one fetch for each daemon process"
    );
}

/// Scenario: With the orchestration selected in the "Interrupted work" list,
/// a refresh drops an earlier item, adds a newer one or reorders the list.
/// The selection stays on the orchestration (`▶ release-team`), and `d`
/// dismisses it; only when the selected item is gone does the selection
/// move to its neighbour.
#[spec("journal/interrupted/021")]
#[test]
fn interrupted_021_a_refresh_keeps_the_selected_item() {
    let newer = item(NEW_ID, ItemKind::Unit, "newer-work");
    let mut agent2 = agent();
    agent2.id = WorkId::parse("w-abcdefgh-0000000000000005").unwrap();
    agent2.name = Some("other-shell".to_string());
    for (what, refreshed) in [
        ("an earlier item dismissed", list_of(vec![agent(), team()])),
        (
            "a newer item added",
            list_of(vec![agent(), newer.clone(), unit(), team()]),
        ),
        ("reordered", list_of(vec![agent(), team(), unit()])),
    ] {
        let (shown, sent) = run(
            &three(),
            &[
                ch('I'),
                ch('j'),
                InterruptedSeamStep::Listed(refreshed),
                ch('d'),
            ],
        );
        assert!(
            shown[3].contains("▶ release-team"),
            "{what}: the selection stays on the orchestration:\n{}",
            shown[3]
        );
        assert_eq!(sent.last(), Some(&dismissal(&[TEAM_ID])), "{what}");
    }
    // The selected plain agent, after another agent is listed before it.
    let (shown, sent) = run(
        &three(),
        &[
            ch('I'),
            ch('j'),
            ch('j'),
            InterruptedSeamStep::Listed(list_of(vec![agent2, agent(), unit(), team()])),
            ch('d'),
        ],
    );
    assert!(shown[4].contains("▶ scratch-shell"), "{}", shown[4]);
    assert_eq!(sent.last(), Some(&dismissal(&[AGENT_ID])));
    // Gone: its neighbour.
    let (shown, sent) = run(
        &three(),
        &[
            ch('I'),
            ch('j'),
            InterruptedSeamStep::Listed(list_of(vec![agent(), unit()])),
            ch('d'),
        ],
    );
    assert!(shown[3].contains("▶ scratch-shell"), "{}", shown[3]);
    assert_eq!(sent.last(), Some(&dismissal(&[AGENT_ID])));
}

/// Scenario: With the "Interrupted work" list open, its details or a
/// confirmation, the footer offers `[Hide X]`; pressing `X` hides the footer
/// row and dismisses nothing, and with the hide key remapped to `Z` the
/// footer offers `[Hide Z]` and `Z` hides it.
#[spec("journal/interrupted/022")]
#[test]
fn interrupted_022_the_hide_key_acts_in_the_overlay() {
    let footer = interrupted_footer(3);
    for (what, steps) in [
        ("list", vec![ch('I'), ch('X')]),
        ("details", vec![ch('I'), key(KeyCode::Enter), ch('X')]),
        ("dismiss all", vec![ch('I'), ch('D'), ch('X')]),
    ] {
        let (shown, sent) = run(&three(), &steps);
        let before = &shown[shown.len() - 2];
        let after = shown.last().unwrap();
        assert!(
            before.contains(&footer) && before.contains("[Hide X]"),
            "{what}: {before}"
        );
        // The dismiss-all question repeats the footer's sentence; its
        // buttons are the footer's own.
        assert!(
            !after.contains("[Hide X]")
                && !after.contains("[Show I]")
                && after.contains(INTERRUPTED_WORK_TITLE),
            "{what}: `X` hides the footer and leaves the overlay open:\n{after}"
        );
        assert!(
            !sent
                .iter()
                .any(|r| matches!(r, InterruptedSeamRequest::Dismiss { .. })),
            "{what}: {sent:?}"
        );
    }
    let (remapped, warnings) =
        KeybindingConfig::from_toml_str("[dashboard]\nhide_interrupted_work = \"Z\"\n")
            .expect("the remap parses");
    assert!(warnings.is_empty(), "{warnings:?}");
    let (shown, _) = run_with(&three(), &remapped, &[ch('I'), ch('Z')], WIDTH, HEIGHT);
    assert!(shown[1].contains("[Hide Z]"), "{}", shown[1]);
    assert!(!shown[2].contains(&footer), "{}", shown[2]);
}

/// Scenario: In a unit's details, `Enter` asks for its kept reports and the
/// request fails, or the daemon does not support it. The details say why,
/// `Enter` asks again, and the next answer shows the report. A failed `n`
/// for the next page says why too, keeps the page shown, and `n` asks again.
#[spec("journal/interrupted/023")]
#[test]
fn interrupted_023_a_failed_report_request_is_shown_and_can_be_retried() {
    let good = page(1, 1, 1, Some(1));
    for (what, failure, reason) in [
        (
            "failed",
            InterruptedSeamStep::Failed(InterruptedSeamKind::Show, "the daemon went away".into()),
            "the daemon went away",
        ),
        (
            "unsupported",
            InterruptedSeamStep::Unsupported(InterruptedSeamKind::Show),
            &UNSUPPORTED[..=UNSUPPORTED.find('.').unwrap()],
        ),
    ] {
        let (shown, sent) = run(
            &only(unit()),
            &[
                ch('I'),
                key(KeyCode::Enter),
                key(KeyCode::Enter),
                failure,
                key(KeyCode::Enter),
                InterruptedSeamStep::Shown(good.clone()),
            ],
        );
        assert!(
            shown[4].contains(reason),
            "{what}: the details say why:\n{}",
            shown[4]
        );
        let shows = sent
            .iter()
            .filter(|r| matches!(r, InterruptedSeamRequest::Show { .. }))
            .count();
        assert_eq!(shows, 2, "{what}: `Enter` asks again: {sent:?}");
        assert!(shown[6].contains("BODY OF r-1"), "{what}:\n{}", shown[6]);
    }

    let mut listed = unit();
    listed.reports = (1..=3).map(report).collect();
    listed.reports_omitted = 5;
    let (shown, sent) = run(
        &only(listed),
        &[
            ch('I'),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            InterruptedSeamStep::Shown(page(1, 3, 8, Some(1))),
            ch('n'),
            InterruptedSeamStep::Failed(InterruptedSeamKind::Show, "timed out".into()),
            ch('n'),
        ],
    );
    let failed = &shown[6];
    assert!(
        failed.contains("timed out") && failed.contains("Reports 1 to 3 of 8."),
        "a failed page says why and keeps the page shown:\n{failed}"
    );
    assert_eq!(
        sent.last(),
        Some(&InterruptedSeamRequest::Show {
            id: UNIT_ID.to_string(),
            reports_from: 3,
        }),
        "`n` asks again: {sent:?}"
    );
}

/// Scenario: In a 40-column terminal, `D` asks "Dismiss all: 3 interrupted
/// items from before the restart? [y/N]" wrapped over several rows, and `y`
/// dismisses all three. In an 80×24 terminal, a dry run naming long Chinese
/// paths pushes "Reclaim? y/n" below the popup: `y` does nothing until
/// PageDown scrolls the question into view, then reclaims. `Esc` and `q`
/// still go back.
#[spec("journal/interrupted/024")]
#[test]
fn interrupted_024_a_confirmation_too_big_for_its_popup_can_be_reached() {
    let all = dismissal(&[UNIT_ID, TEAM_ID, AGENT_ID]);
    let (shown, sent) = run_with(
        &three(),
        &KeybindingConfig::default(),
        &[ch('I'), ch('D'), ch('y')],
        40,
        24,
    );
    let asked: String = shown[2]
        .lines()
        .filter_map(|line| line.split('│').nth(1))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        asked.contains(dismiss_all_question(3).trim_end()),
        "the whole question is drawn:\n{}",
        shown[2]
    );
    assert_eq!(sent.last(), Some(&all), "{}", shown[3]);
    for back in [key(KeyCode::Esc), ch('q')] {
        let (shown, sent) = run_with(
            &three(),
            &KeybindingConfig::default(),
            &[ch('I'), ch('D'), back],
            40,
            24,
        );
        assert!(!sent.iter().any(is_dismissal) && !shown[3].contains("Dismiss all:"));
    }

    let long = |n: usize| format!("/home/me/代码/{}/{n}", "非常长的目录名称".repeat(6));
    let dry = ReclaimOutcome {
        worktree: Some(long(0)),
        branch: Some(format!("agent/{}", "分支".repeat(40))),
        unpushed_commits: Some(3),
        nested_worktrees: (1..=6).map(long).collect(),
        ..would_remove()
    };
    let (shown, sent) = run_with(
        &only(unit()),
        &KeybindingConfig::default(),
        &[
            ch('I'),
            ch('r'),
            InterruptedSeamStep::Reclaimed(dry.clone()),
            ch('y'),
            key(KeyCode::PageDown),
            key(KeyCode::PageDown),
            key(KeyCode::PageDown),
            key(KeyCode::PageDown),
            ch('y'),
        ],
        80,
        24,
    );
    assert!(
        !shown[3].contains("Reclaim? y/n"),
        "the premise: the question is below the popup:\n{}",
        shown[3]
    );
    assert_eq!(
        sent.iter().filter(|r| is_real_reclaim(r)).count(),
        1,
        "`y` reclaims once, after the question is in view: {sent:?}"
    );
    assert!(shown[8].contains("Reclaim? y/n"), "{}", shown[8]);
    let (_, before_scroll) = run_with(
        &only(unit()),
        &KeybindingConfig::default(),
        &[
            ch('I'),
            ch('r'),
            InterruptedSeamStep::Reclaimed(dry.clone()),
            ch('y'),
        ],
        80,
        24,
    );
    assert!(
        !before_scroll.iter().any(is_real_reclaim),
        "`y` before the question is in view does nothing: {before_scroll:?}"
    );
    for back in [key(KeyCode::Esc), ch('q')] {
        let (shown, _) = run_with(
            &only(unit()),
            &KeybindingConfig::default(),
            &[
                ch('I'),
                ch('r'),
                InterruptedSeamStep::Reclaimed(dry.clone()),
                back,
            ],
            80,
            24,
        );
        assert!(
            shown[4].contains("fix-login") && !shown[4].contains(RECLAIM_EXPLANATION),
            "{}",
            shown[4]
        );
    }
}

/// Scenario: A `y` typed before the confirmation was drawn, or while a
/// terminal too short shows no question, sends nothing; a dry run that keeps
/// the worktree or was refused offers no `y`, and `y` sends no reclaim; and
/// `D`, a refresh that lists a fourth item, then `y` dismisses exactly the
/// three items the question named, never "all".
#[spec("journal/interrupted/025")]
#[test]
fn interrupted_025_a_confirmation_sends_only_what_it_showed() {
    let all = dismissal(&[UNIT_ID, TEAM_ID, AGENT_ID]);
    // Typed before a frame showed the question.
    let (_, sent) = run(
        &three(),
        &[ch('I'), InterruptedSeamStep::Burst(vec![ch('D'), ch('y')])],
    );
    assert!(!sent.iter().any(is_dismissal), "{sent:?}");
    let (_, sent) = run(
        &only(unit()),
        &[
            ch('I'),
            ch('r'),
            InterruptedSeamStep::Burst(vec![
                InterruptedSeamStep::Reclaimed(would_remove()),
                ch('y'),
            ]),
        ],
    );
    assert!(!sent.iter().any(is_real_reclaim), "{sent:?}");
    // A terminal too short to show the question at all.
    let (_, sent) = run_with(
        &three(),
        &KeybindingConfig::default(),
        &[ch('I'), ch('D'), ch('y'), key(KeyCode::PageDown), ch('y')],
        WIDTH,
        7,
    );
    assert!(!sent.iter().any(is_dismissal), "{sent:?}");
    // Kept and refused dry runs.
    for outcome in [
        ReclaimOutcome {
            verdict: ReclaimVerdict::KeptDirty,
            ..would_remove()
        },
        ReclaimOutcome {
            verdict: ReclaimVerdict::Refused,
            refusal: Some(Refusal::new(RefusalReason::NotOwned)),
            ..would_remove()
        },
        ReclaimOutcome {
            refusal: Some(Refusal::new(RefusalReason::StillInUse)),
            ..would_remove()
        },
    ] {
        let (shown, sent) = run(
            &only(unit()),
            &[
                ch('I'),
                ch('r'),
                InterruptedSeamStep::Reclaimed(outcome),
                ch('y'),
            ],
        );
        assert!(!shown[3].contains("y/n"), "{}", shown[3]);
        assert!(!sent.iter().any(is_real_reclaim), "{sent:?}");
    }
    // Exactly the ids the question named, with `all` false.
    let newer = item(NEW_ID, ItemKind::Unit, "newer-work");
    let (shown, sent) = run(
        &three(),
        &[
            ch('I'),
            ch('D'),
            InterruptedSeamStep::Listed(list_of(vec![agent(), newer, unit(), team()])),
            ch('y'),
        ],
    );
    assert!(
        shown[3].contains(dismiss_all_question(3).trim_end()),
        "{}",
        shown[3]
    );
    assert_eq!(sent.last(), Some(&all), "{sent:?}");
}

/// Scenario: A unit's details show "Reports 1 to 3 of 8."; then all but two
/// of its reports expire, and `n` asks for the page from the fourth. The
/// daemon answers the page clamped to where the two reports end, with no
/// report on it. The details apply that answer: "Reports 1 to 3 of 8." and
/// `n next reports` are gone and `p previous reports` is offered; the first
/// page's late answer is not shown, and `p` reaches the two kept reports.
#[spec("journal/interrupted/026")]
#[test]
fn interrupted_026_a_clamped_report_page_is_applied_not_dropped_as_stale() {
    let mut listed = unit();
    listed.reports = (1..=3).map(report).collect();
    listed.reports_omitted = 5;
    // The daemon clamps the asked-for offset (3) to the two reports left.
    let mut clamped = page(1, 2, 2, None);
    clamped.item.reports = Vec::new();
    clamped.item.reports_omitted = 2;
    clamped.reports_from = 2;
    clamped.next_reports_from = None;
    let (shown, sent) = run(
        &only(listed),
        &[
            ch('I'),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            InterruptedSeamStep::Shown(page(1, 3, 8, Some(1))),
            ch('n'),
            InterruptedSeamStep::Shown(clamped),
            // The first page's request, answered again, late.
            InterruptedSeamStep::AnswerTo(
                1,
                Box::new(InterruptedSeamStep::Shown(page(1, 3, 8, Some(1)))),
            ),
            ch('p'),
            InterruptedSeamStep::Shown(page(1, 2, 2, Some(1))),
        ],
    );
    let show = |from: usize| InterruptedSeamRequest::Show {
        id: UNIT_ID.to_string(),
        reports_from: from,
    };
    assert_eq!(
        sent.iter()
            .filter(|r| matches!(r, InterruptedSeamRequest::Show { .. }))
            .cloned()
            .collect::<Vec<_>>(),
        vec![show(0), show(3), show(0)],
        "{sent:?}"
    );
    at(&shown[4], "Reports 1 to 3 of 8.");
    for frame in [&shown[6], &shown[7]] {
        assert!(
            !frame.contains("Reports 1 to 3 of 8.") && !frame.contains("n next reports"),
            "the clamped page replaces the stale one:\n{frame}"
        );
        at(frame, "p previous reports");
    }
    let back = &shown[9];
    at(back, &format!("{LABEL_REPORT}: r-2"));
    at(back, "BODY OF r-1");
    assert!(!back.contains("n next reports"), "{back}");
}

/// What the rows of the popup in `frame` hold between its borders, with
/// every space removed, since a wrap may split a word across rows.
fn popup_text(frame: &str) -> String {
    frame
        .lines()
        .filter_map(|line| line.split('│').nth(1))
        .flat_map(|row| row.chars().filter(|c| !c.is_whitespace()))
        .collect()
}

/// Scenario: In a 14×24 terminal, `D` asks "Dismiss all: 3 interrupted items
/// from before the restart? [y/N]" wrapped over nine rows, every one of them
/// drawn, and `y` dismisses exactly the three listed items. In a terminal too
/// short to show the whole question at once, `y` does nothing until the
/// person has scrolled through every row of it, even though no single frame
/// showed it all; a scroll key and `y` typed together before a frame drew the
/// rest send nothing.
#[spec("journal/interrupted/027")]
#[test]
fn interrupted_027_a_question_seen_whole_can_be_confirmed_whatever_its_rows() {
    let all = dismissal(&[UNIT_ID, TEAM_ID, AGENT_ID]);
    let question: String = dismiss_all_question(3)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let (shown, sent) = run_with(
        &three(),
        &KeybindingConfig::default(),
        &[ch('I'), ch('D'), ch('y')],
        14,
        24,
    );
    let rows = shown[2]
        .lines()
        .filter_map(|line| line.split('│').nth(1))
        .filter(|row| !row.trim().is_empty())
        .count();
    assert!(
        rows > 8 && popup_text(&shown[2]).contains(&question),
        "the premise: the whole question is drawn, over more than eight rows:\n{}",
        shown[2]
    );
    assert_eq!(sent.last(), Some(&all), "{sent:?}\n{}", shown[3]);
    assert_eq!(sent.iter().filter(|r| is_dismissal(r)).count(), 1);

    // Too short for the whole question at once: nothing until every row was
    // drawn, then `y` dismisses.
    let height = 10;
    let (shown, sent) = run_with(
        &three(),
        &KeybindingConfig::default(),
        &[ch('I'), ch('D'), ch('y')],
        14,
        height,
    );
    assert!(
        !popup_text(&shown[2]).contains(&question),
        "the premise: one frame cannot show the whole question:\n{}",
        shown[2]
    );
    assert!(!sent.iter().any(is_dismissal), "{sent:?}");
    let mut steps = vec![ch('I'), ch('D')];
    steps.extend((0..12).map(|_| ch('j')));
    steps.push(ch('y'));
    let (shown, sent) = run_with(&three(), &KeybindingConfig::default(), &steps, 14, height);
    assert_eq!(
        sent.iter().filter(|r| is_dismissal(r)).collect::<Vec<_>>(),
        vec![&all],
        "every row was drawn by some frame: {sent:?}\n{}",
        shown.last().unwrap()
    );
    // Scrolled back to the top after seeing it all: still seen.
    let mut steps = vec![ch('I'), ch('D')];
    steps.extend((0..12).map(|_| ch('j')));
    steps.extend((0..12).map(|_| ch('k')));
    steps.push(ch('y'));
    let (_, sent) = run_with(&three(), &KeybindingConfig::default(), &steps, 14, height);
    assert_eq!(sent.last(), Some(&all), "{sent:?}");
    // Scroll keys and `y` in one burst, before a frame drew the lower rows.
    let mut burst: Vec<InterruptedSeamStep> = (0..12).map(|_| ch('j')).collect();
    burst.push(ch('y'));
    let (_, sent) = run_with(
        &three(),
        &KeybindingConfig::default(),
        &[ch('I'), ch('D'), InterruptedSeamStep::Burst(burst)],
        14,
        height,
    );
    assert!(!sent.iter().any(is_dismissal), "{sent:?}");
}

// ---------------------------------------------------------------------------
// Dismiss all in batches (final review, reviewer 1 / auditor 2)
// ---------------------------------------------------------------------------

/// More items than one dismissal may name: the daemon admits at most 256 ids
/// in one request (`close_agents::MAX_SELECTOR_ENTRIES`).
const MANY: usize = 257;

fn many_ids() -> Vec<String> {
    (0..MANY)
        .map(|n| format!("w-abcdefgh-{:016}", 1000 + n))
        .collect()
}

fn many() -> InterruptedList {
    list_of(
        many_ids()
            .iter()
            .enumerate()
            .map(|(n, id)| item(id, ItemKind::Unit, &format!("unit-{n}")))
            .collect(),
    )
}

fn dismissed_all(ids: &[String]) -> DismissOutcome {
    DismissOutcome {
        dismissed: ids
            .iter()
            .map(|id| DismissedItem {
                id: id.clone(),
                name: None,
            })
            .collect(),
        ..DismissOutcome::default()
    }
}

fn dismissals(sent: &[InterruptedSeamRequest]) -> Vec<(Vec<String>, bool)> {
    sent.iter()
        .filter_map(|request| match request {
            InterruptedSeamRequest::Dismiss { ids, all } => Some((ids.clone(), *all)),
            _ => None,
        })
        .collect()
}

/// `sentence` without its whitespace, to find it in [`popup_text`], however
/// the popup wrapped it.
fn squeezed(sentence: &str) -> String {
    sentence.chars().filter(|c| !c.is_whitespace()).collect()
}

fn lists_sent(sent: &[InterruptedSeamRequest]) -> usize {
    sent.iter()
        .filter(|r| matches!(r, InterruptedSeamRequest::List { .. }))
        .count()
}

/// Scenario: The list holds 257 interrupted items, one more than the daemon
/// takes in one dismissal. `I`, `D` and `y` send two dismissals, of 256 and 1
/// ids, the second only once the first answered, together naming exactly the
/// 257 items the question named and never "all"; the list is asked for again
/// only after the last answer, the overlay says "Dismissed.", and an item that
/// appeared after the question stays listed.
#[spec("journal/interrupted/029")]
#[test]
fn interrupted_029_dismiss_all_sends_bounded_batches_of_exactly_the_confirmed_ids() {
    let ids = many_ids();
    let confirm = || vec![ch('I'), ch('D'), ch('y')];
    let (_, sent) = run(&many(), &confirm());
    let batches = dismissals(&sent);
    assert_eq!(batches.len(), 1, "one batch at a time: {batches:?}");
    assert_eq!(batches[0].0.len(), 256);
    assert!(!batches[0].1, "never all");

    let mut steps = confirm();
    steps.push(InterruptedSeamStep::Dismissed(dismissed_all(&ids[..256])));
    let (_, sent) = run(&many(), &steps);
    let batches = dismissals(&sent);
    assert_eq!(batches.len(), 2, "{batches:?}");
    assert_eq!(batches[1].0, ids[256..].to_vec());
    assert!(!batches[1].1, "never all");
    let named: Vec<String> = batches.iter().flat_map(|(ids, _)| ids.clone()).collect();
    assert_eq!(named, ids, "together, exactly the confirmed ids, in order");
    assert_eq!(
        lists_sent(&sent),
        2,
        "no refresh between batches (the attach fetch and `I`): {sent:?}"
    );

    let newer = item(NEW_ID, ItemKind::Unit, "newer-work");
    steps.push(InterruptedSeamStep::Dismissed(dismissed_all(&ids[256..])));
    steps.push(InterruptedSeamStep::Listed(only(newer)));
    let (shown, sent) = run(&many(), &steps);
    assert_eq!(dismissals(&sent).len(), 2, "{sent:?}");
    assert_eq!(lists_sent(&sent), 3, "one refresh, after the last answer");
    let done = &shown[shown.len() - 2];
    assert!(done.contains(DISMISSED), "{done}");
    let refreshed = shown.last().unwrap();
    assert!(
        refreshed.contains("newer-work") && refreshed.contains(&interrupted_footer(1)),
        "the item listed after the question stays listed:\n{refreshed}"
    );
}

/// Scenario: Dismiss all over 257 items; the first batch of 256 is dismissed
/// and the second fails (a timeout), or is refused whole. No third request is
/// sent, the list is asked for again, and the overlay says "Dismissed." and
/// that 1 more item was not dismissed, and why. When the first batch also
/// refused one of its ids, the overlay says that refusal and still says the
/// later batch stopped.
#[spec("journal/interrupted/030")]
#[test]
fn interrupted_030_a_failed_later_batch_stops_dismiss_all_and_says_so() {
    let ids = many_ids();
    // Verification finding B: a first batch that dismissed 255 and refused
    // one, then a later batch that failed. Both facts are said.
    let mut partly = dismissed_all(&ids[..255]);
    partly.push_refused(ids[255].clone(), RefusalReason::NotListed);
    let (shown, sent) = run(
        &many(),
        &[
            ch('I'),
            ch('D'),
            ch('y'),
            InterruptedSeamStep::Dismissed(partly),
            InterruptedSeamStep::Failed(InterruptedSeamKind::Dismiss, "timed out".to_string()),
        ],
    );
    assert_eq!(dismissals(&sent).len(), 2, "{sent:?}");
    let said = popup_text(shown.last().unwrap());
    let refused = squeezed(refusal_sentence(RefusalReason::NotListed));
    let why = squeezed(&dismiss_stopped_sentence(&DismissStopped {
        reason: DismissStopReason::Failed,
        refusal: None,
        not_sent: 1,
    }));
    assert!(said.contains(&refused), "the refusal, in:\n{said}");
    assert!(said.contains(&why), "and the stopped batch, in:\n{said}");

    let first = || InterruptedSeamStep::Dismissed(dismissed_all(&ids[..256]));
    for (last, stopped) in [
        (
            InterruptedSeamStep::Failed(InterruptedSeamKind::Dismiss, "timed out".to_string()),
            DismissStopped {
                reason: DismissStopReason::Failed,
                refusal: None,
                not_sent: 1,
            },
        ),
        (
            InterruptedSeamStep::Dismissed(DismissOutcome {
                refusal: Some(Refusal::new(RefusalReason::Unreadable)),
                ..DismissOutcome::default()
            }),
            DismissStopped {
                reason: DismissStopReason::Refused,
                refusal: Some(RefusalReason::Unreadable),
                not_sent: 1,
            },
        ),
    ] {
        let (shown, sent) = run(&many(), &[ch('I'), ch('D'), ch('y'), first(), last]);
        assert_eq!(dismissals(&sent).len(), 2, "{sent:?}");
        assert_eq!(
            lists_sent(&sent),
            3,
            "the list is asked for again: {sent:?}"
        );
        let said = shown.last().unwrap();
        assert!(said.contains(DISMISSED), "{said}");
        let why = dismiss_stopped_sentence(&stopped);
        assert!(
            popup_text(said).contains(&squeezed(&why)),
            "{why:?} in:\n{said}"
        );
    }
}

/// Scenario: Dismiss all over 257 items; after the first batch answers, the
/// daemon process is replaced — the second batch is withheld from the new
/// process (unsupported), or the TUI's event stream attaches to the new
/// process before it answers. No further dismissal is sent to the new
/// process, and the overlay says that 1 more item was not dismissed because
/// the deck daemon was replaced.
#[spec("journal/interrupted/031")]
#[test]
fn interrupted_031_a_replaced_daemon_stops_dismiss_all_between_batches() {
    let ids = many_ids();
    let first = || InterruptedSeamStep::Dismissed(dismissed_all(&ids[..256]));
    let why = dismiss_stopped_sentence(&DismissStopped {
        reason: DismissStopReason::Replaced,
        refusal: None,
        not_sent: 1,
    });
    for replaced in [
        vec![InterruptedSeamStep::Unsupported(
            InterruptedSeamKind::Dismiss,
        )],
        vec![
            InterruptedSeamStep::Attach(SECOND_DAEMON.to_string()),
            InterruptedSeamStep::AnswerTo(
                3,
                Box::new(InterruptedSeamStep::Dismissed(dismissed_all(&ids[256..]))),
            ),
        ],
    ] {
        let mut steps = vec![ch('I'), ch('D'), ch('y'), first()];
        steps.extend(replaced);
        let run = run_interrupted_work_seam(
            &many(),
            &[],
            UiMode::Normal,
            &KeybindingConfig::default(),
            &steps,
            WIDTH,
            HEIGHT,
        );
        let requests: Vec<InterruptedSeamRequest> =
            run.sent.iter().map(|s| s.request.clone()).collect();
        assert_eq!(dismissals(&requests).len(), 2, "{requests:?}");
        assert!(
            run.sent
                .iter()
                .filter(|s| is_dismissal(&s.request))
                .all(|s| s.instance.as_deref() == Some(INTERRUPTED_SEAM_FIRST_DAEMON)),
            "every batch is pinned to the process the question was asked of: {:?}",
            run.sent
        );
        let said = text(run.frames.last().unwrap());
        assert!(
            popup_text(&said).contains(&squeezed(&why)),
            "{why:?} in:\n{said}"
        );
    }
}

/// Scenario: The daemon refuses a dismissal whole because it could not read
/// its whole work journal. `d` on one item, and `D` `y` over 257 items whose
/// first batch is refused, each show "Refused: the deck could not read its
/// whole work journal, …" and send nothing more; when only the second batch
/// is refused, the overlay says "Dismissed." and that 1 more item was not
/// dismissed, with the same refusal.
#[spec("journal/interrupted/032")]
#[test]
fn interrupted_032_a_dismissal_refused_on_an_incomplete_read_says_so() {
    let incomplete = || {
        InterruptedSeamStep::Dismissed(DismissOutcome {
            refusal: Some(Refusal::new(RefusalReason::IncompleteRead)),
            ..DismissOutcome::default()
        })
    };
    let refused = squeezed(refusal_sentence(RefusalReason::IncompleteRead));

    let (shown, sent) = run(&three(), &[ch('I'), ch('d'), incomplete()]);
    assert_eq!(dismissals(&sent).len(), 1, "{sent:?}");
    let said = popup_text(shown.last().unwrap());
    assert!(said.contains(&refused), "one item:\n{said}");

    let (shown, sent) = run(&many(), &[ch('I'), ch('D'), ch('y'), incomplete()]);
    assert_eq!(dismissals(&sent).len(), 1, "no second batch: {sent:?}");
    let said = popup_text(shown.last().unwrap());
    assert!(said.contains(&refused), "the first batch:\n{said}");

    let ids = many_ids();
    let (shown, sent) = run(
        &many(),
        &[
            ch('I'),
            ch('D'),
            ch('y'),
            InterruptedSeamStep::Dismissed(dismissed_all(&ids[..256])),
            incomplete(),
        ],
    );
    assert_eq!(dismissals(&sent).len(), 2, "{sent:?}");
    let said = popup_text(shown.last().unwrap());
    let why = squeezed(&dismiss_stopped_sentence(&DismissStopped {
        reason: DismissStopReason::Refused,
        refusal: Some(RefusalReason::IncompleteRead),
        not_sent: 1,
    }));
    assert!(said.contains(&squeezed(DISMISSED)), "{said}");
    assert!(said.contains(&why), "a later batch:\n{said}");
}

/// Scenario: The daemon answers with an empty list that says its work journal
/// could not be read whole. The dashboard's footer still shows a row, saying
/// some interrupted work may not be listed, with `[Show I]` and `[Hide X]`;
/// `I` opens the "Interrupted work" list, which says the journal could not be
/// read whole, and `X` hides the row; `I` pressed after `X` still opens the
/// list. A complete empty list shows no row.
#[spec("journal/interrupted/033")]
#[test]
fn interrupted_033_an_incomplete_empty_list_still_offers_the_footer_row() {
    let unread = InterruptedList {
        items: Vec::new(),
        incomplete: true,
        omitted: 0,
    };
    let hidden = frames(&unread, &[ch('X')]);
    assert!(hidden[0].contains(INTERRUPTED_UNREAD), "{}", hidden[0]);
    assert!(hidden[0].contains("[Show I]"), "{}", hidden[0]);
    assert!(hidden[0].contains("[Hide X]"), "{}", hidden[0]);
    insta::assert_snapshot!("footer_incomplete_empty_list", hidden[0]);
    assert!(
        !hidden[1].contains(INTERRUPTED_UNREAD) && !hidden[1].contains("[Hide X]"),
        "`X` hides the row:\n{}",
        hidden[1]
    );

    let (opened, sent) = run(&unread, &[ch('I')]);
    let open = opened.last().unwrap();
    assert!(open.contains(INTERRUPTED_WORK_TITLE), "{open}");
    assert!(
        popup_text(open).contains(&squeezed(LIST_INCOMPLETE)),
        "{open}"
    );
    assert!(
        sent.iter()
            .any(|r| matches!(r, InterruptedSeamRequest::List { git: true })),
        "`I` asks for the list again: {sent:?}"
    );

    // The TUI has one daemon, so a hidden row never leaves `I` pointing at a
    // banner that is not drawn: `I` opens the list itself (verification R2).
    let (after_hide, _) = run(&unread, &[ch('X'), ch('I')]);
    let reopened = after_hide.last().unwrap();
    assert!(
        reopened.contains(INTERRUPTED_WORK_TITLE)
            && popup_text(reopened).contains(&squeezed(LIST_INCOMPLETE)),
        "`I` opens the list after `X` hid the row:\n{reopened}"
    );

    let complete = frames(&InterruptedList::default(), &[]);
    assert!(
        !complete[0].contains(INTERRUPTED_UNREAD) && !complete[0].contains("[Show I]"),
        "a complete empty list, no row:\n{}",
        complete[0]
    );
}

/// Scenario: The list was read with each item's Git facts, and the unit's
/// details show its "Git:" line. Pressing `Enter` in the details asks for its
/// kept reports; the daemon's answer carries the item without Git facts. The
/// details, now showing the report's text, still show the same "Git:" line.
#[spec("journal/interrupted/034")]
#[test]
fn interrupted_034_opening_the_reports_keeps_the_items_git_facts() {
    let mut listed = unit();
    listed.git = Some(GitFacts {
        worktree_exists: Some(true),
        dirty: Some(true),
        unpushed_commits: Some(2),
        branch_exists: Some(true),
        pr: PrFact::Open,
    });
    let git_line = format!(
        "{LABEL_GIT}: {}",
        git_facts_line(listed.git.as_ref().unwrap())
    );
    let mut answer = page(1, 1, 1, Some(1));
    answer.item.git = None;
    let shown = frames(
        &only(listed),
        &[
            ch('I'),
            key(KeyCode::Enter),
            key(KeyCode::Enter),
            InterruptedSeamStep::Shown(answer),
        ],
    );
    let details = popup_text(&shown[2]);
    assert!(details.contains(&squeezed(&git_line)), "{}", shown[2]);
    let reports = &shown[4];
    assert!(
        reports.contains("BODY OF r-1"),
        "the report is shown:\n{reports}"
    );
    assert!(
        popup_text(reports).contains(&squeezed(&git_line)),
        "the report page keeps the list's Git facts:\n{reports}"
    );
}

/// Scenario: A deck restart stopped two agents and the interrupted work list
/// came back empty but incomplete. The restart notice still adds its line
/// leading to the list, saying some interrupted work may not be listed, and
/// pressing `I` there opens "Interrupted work", which says the list is
/// incomplete.
#[spec("journal/interrupted/035")]
#[test]
fn interrupted_035_the_restart_notice_leads_to_an_incomplete_empty_list() {
    let unread = InterruptedList {
        items: Vec::new(),
        incomplete: true,
        omitted: 0,
    };
    let shown: Vec<String> = render_interrupted_work_to_buffers(
        &unread,
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
    at(notice, "The deck restarted; these agents were stopped:");
    assert!(
        popup_text(notice).contains(&squeezed(INTERRUPTED_UNREAD)),
        "the notice's line says some work may not be listed:\n{notice}"
    );
    assert!(
        shown[1].contains(INTERRUPTED_WORK_TITLE)
            && popup_text(&shown[1]).contains(&squeezed(LIST_INCOMPLETE)),
        "`I` opens the interrupted work list:\n{}",
        shown[1]
    );
}

/// Scenario: The "Interrupted work" list is open with three items, and asking
/// for it again fails (a timeout). The overlay still shows the three items and
/// now says the list could not be refreshed and may be out of date, with why.
/// Closed with `Esc` and opened again with `I`, it still says so until the new
/// request answers; then the sentence is gone.
#[spec("journal/interrupted/037")]
#[test]
fn interrupted_037_a_failed_refresh_says_the_list_may_be_out_of_date() {
    let (shown, _) = run(
        &three(),
        &[
            ch('I'),
            InterruptedSeamStep::Failed(InterruptedSeamKind::List, "timed out".into()),
            key(KeyCode::Esc),
            ch('I'),
            InterruptedSeamStep::Listed(three()),
        ],
    );
    let open = popup_text(&shown[1]);
    assert!(
        !open.contains(&squeezed(LIST_REFRESH_FAILED)),
        "{}",
        shown[1]
    );
    let failed = popup_text(&shown[2]);
    assert!(
        failed.contains(&squeezed(LIST_REFRESH_FAILED)) && failed.contains("timedout"),
        "a failed refresh says so, and why:\n{}",
        shown[2]
    );
    assert!(
        shown[2].contains("fix-login") && shown[2].contains("release-team"),
        "the list it had is still shown:\n{}",
        shown[2]
    );
    assert!(
        popup_text(&shown[4]).contains(&squeezed(LIST_REFRESH_FAILED)),
        "reopened, it still says so until a list answers:\n{}",
        shown[4]
    );
    assert!(
        !popup_text(&shown[5]).contains(&squeezed(LIST_REFRESH_FAILED)),
        "a list that answers clears it:\n{}",
        shown[5]
    );
}
