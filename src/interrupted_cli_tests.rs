use super::*;
use crate::interrupted_work::{
    DismissRefused, DismissedItem, GitFacts, ItemReport, LastEntry, PrFact, Refusal, ShownReport,
};
use crate::work_journal::WorkId;

fn item(id: &str, kind: ItemKind, name: &str) -> InterruptedItem {
    InterruptedItem {
        id: WorkId::parse(id).unwrap(),
        kind,
        name: Some(name.to_string()),
        issue: None,
        repo: None,
        branch: (kind != ItemKind::Agent).then(|| format!("agent/dispatch-{name}")),
        worktree: Some(format!("/work/{name}")),
        launcher: (kind != ItemKind::Agent).then(|| "caller".to_string()),
        last_entry: LastEntry {
            kind: "attempt_ended".into(),
            ts_ms: 1_795_000_000_000,
        },
        reports: Vec::new(),
        reports_omitted: 0,
        cut: false,
        progress: Default::default(),
        git: None,
    }
}

const UNIT_ID: &str = "w-abcdefgh-0000000000000001";
const AGENT_ID: &str = "w-abcdefgh-0000000000000002";

#[test]
fn the_list_puts_plain_agents_under_their_own_heading_after_the_work() {
    let mut unit = item(UNIT_ID, ItemKind::Unit, "fix-login");
    unit.reports.push(ItemReport {
        report_id: "r-1".into(),
        task_id: None,
        unit_id: None,
        kept_until_ms: Some(1_795_600_000_000),
    });
    let list = InterruptedList {
        // The daemon's order, newest first, puts the agent first: the text
        // still lists it after the work.
        items: vec![item(AGENT_ID, ItemKind::Agent, "shell"), unit],
        incomplete: false,
        omitted: 0,
    };
    let text = render_list(&list);
    let title = text.find(words::INTERRUPTED_WORK_TITLE).unwrap();
    let unit_at = text.find(UNIT_ID).unwrap();
    let group = text.find(words::OTHER_AGENTS_GROUP).unwrap();
    let agent_at = text.find(AGENT_ID).unwrap();
    assert!(
        title < unit_at && unit_at < group && group < agent_at,
        "{text}"
    );
    assert!(text.contains("Branch: agent/dispatch-fix-login"), "{text}");
    assert!(text.contains("Started by: caller"), "{text}");
    assert!(text.contains("Folder: /work/shell"), "{text}");
    assert!(
        text.contains(&format!("r-1, {} ", words::KEPT_UNTIL)),
        "{text}"
    );
    assert!(
        text.trim_end().ends_with(&words::interrupted_footer(2)),
        "{text}"
    );
}

#[test]
fn an_empty_list_says_so_and_has_no_footer() {
    let text = render_list(&InterruptedList::default());
    assert!(text.contains(words::NOTHING_INTERRUPTED), "{text}");
    assert!(!text.contains("from before the restart"), "{text}");
}

#[test]
fn an_incomplete_list_and_omitted_items_are_said() {
    let list = InterruptedList {
        items: vec![item(UNIT_ID, ItemKind::Unit, "a")],
        incomplete: true,
        omitted: 4,
    };
    let text = render_list(&list);
    assert!(text.contains(words::LIST_INCOMPLETE), "{text}");
    assert!(text.contains(&words::omitted_sentence(4)), "{text}");
}

#[test]
fn values_from_the_journal_cannot_move_the_terminal() {
    let mut unit = item(UNIT_ID, ItemKind::Unit, "evil\u{1b}[2J\nname");
    unit.branch = Some("b\rranch".into());
    let text = render_item(&unit);
    assert!(!text.contains('\u{1b}') && !text.contains('\r'), "{text:?}");
    assert_eq!(text.lines().next().unwrap().matches("evil").count(), 1);
}

#[test]
fn git_facts_are_shown_when_the_list_has_them() {
    let mut unit = item(UNIT_ID, ItemKind::Unit, "a");
    unit.git = Some(GitFacts {
        worktree_exists: Some(true),
        dirty: Some(false),
        unpushed_commits: Some(2),
        branch_exists: Some(true),
        pr: PrFact::Unknown,
    });
    let text = render_item(&unit);
    let facts = words::git_facts_line(unit.git.as_ref().unwrap());
    assert!(text.contains(&format!("Git: {facts}")), "{text}");
}

#[test]
fn show_prints_each_body_as_the_daemon_framed_it() {
    let show = InterruptedShow {
        item: item(UNIT_ID, ItemKind::Unit, "a"),
        reports: vec![
            ShownReport {
                report_id: "r-1".into(),
                body: Some(
                    "[UNTRUSTED-WORKER-REPORT:\nhello\n:END-UNTRUSTED-WORKER-REPORT]\n".into(),
                ),
                cut: true,
                error: None,
            },
            ShownReport {
                report_id: "r-2".into(),
                body: None,
                cut: false,
                error: Some("its file is no longer kept".into()),
            },
        ],
        incomplete: false,
        bodies_omitted: 0,
    };
    let text = render_show(&show);
    assert!(
        text.contains("\nhello\n:END-UNTRUSTED-WORKER-REPORT]\n"),
        "{text}"
    );
    assert!(text.contains(words::REPORT_CUT), "{text}");
    assert!(
        text.contains(&format!(
            "{} its file is no longer kept",
            words::REPORT_UNREADABLE
        )),
        "{text}"
    );
}

fn outcome(verdict: ReclaimVerdict) -> ReclaimOutcome {
    ReclaimOutcome {
        id: Some(UNIT_ID.into()),
        dry_run: false,
        verdict,
        message: "the daemon's words".into(),
        refusal: None,
        worktree: Some("/work/a".into()),
        branch: Some("agent/dispatch-a".into()),
        unpushed_commits: Some(3),
        nested_worktrees: Vec::new(),
        recorded: true,
    }
}

#[test]
fn reclaim_exits_zero_only_for_a_recorded_removal_or_a_dry_run_that_would_remove() {
    let removed = render_reclaim(&outcome(ReclaimVerdict::Removed));
    assert_eq!(removed.code, EXIT_OK);
    assert!(removed.stdout.contains(words::RECLAIM_REMOVED));
    assert!(
        removed
            .stdout
            .contains(&words::unpushed_sentence(3).unwrap()),
        "{}",
        removed.stdout
    );
    let would = render_reclaim(&outcome(ReclaimVerdict::WouldRemove));
    assert_eq!(would.code, EXIT_OK);
    assert!(would.stdout.contains(words::RECLAIM_WOULD_REMOVE));

    let mut unrecorded = outcome(ReclaimVerdict::Removed);
    unrecorded.recorded = false;
    let unrecorded = render_reclaim(&unrecorded);
    assert_eq!(unrecorded.code, EXIT_REFUSED);
    assert!(unrecorded.stderr.contains(words::RECLAIM_NOT_RECORDED));

    for verdict in [
        ReclaimVerdict::KeptDirty,
        ReclaimVerdict::KeptCouldNotCheck,
        ReclaimVerdict::RemoveFailed,
        ReclaimVerdict::TimedOut,
    ] {
        let kept = render_reclaim(&outcome(verdict));
        assert_eq!(kept.code, EXIT_REFUSED, "{verdict:?}");
        assert!(
            kept.stderr.contains(words::reclaim_sentence(verdict)),
            "{verdict:?}: {}",
            kept.stderr
        );
    }
}

#[test]
fn a_kept_holder_names_each_nested_worktree() {
    let mut held = outcome(ReclaimVerdict::KeptHoldsWorktrees);
    held.nested_worktrees = vec!["/work/a/.worktrees/x".into(), "/work/a/.worktrees/y".into()];
    let out = render_reclaim(&held);
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(out.stderr.contains(words::RECLAIM_KEPT_HOLDS_WORKTREES));
    for nested in &held.nested_worktrees {
        assert!(
            out.stderr
                .contains(&format!("{}: {nested}", words::LABEL_NESTED_WORKTREE)),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn a_refused_reclaim_uses_the_shared_sentence() {
    let refused = ReclaimOutcome::refused(None, false, RefusalReason::AgentCaller);
    let out = render_reclaim(&refused);
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(
        out.stderr.contains(words::REFUSED_AGENT_CALLER),
        "{}",
        out.stderr
    );
}

#[test]
fn dismiss_reports_each_item_and_exits_one_when_any_was_refused() {
    let all_done = render_dismiss(&DismissOutcome {
        dismissed: vec![DismissedItem {
            id: UNIT_ID.into(),
            name: Some("a".into()),
        }],
        ..DismissOutcome::default()
    });
    assert_eq!(all_done.code, EXIT_OK);
    assert!(
        all_done
            .stdout
            .contains(&format!("{} {UNIT_ID}", words::DISMISSED)),
        "{}",
        all_done.stdout
    );

    let partly = render_dismiss(&DismissOutcome {
        dismissed: Vec::new(),
        refused: vec![DismissRefused {
            id: "nope".into(),
            refusal: Refusal::new(RefusalReason::NotListed),
        }],
        refusal: None,
        incomplete: false,
        dismissed_omitted: 0,
        refused_omitted: 0,
    });
    assert_eq!(partly.code, EXIT_REFUSED);
    assert!(
        partly
            .stderr
            .contains(&format!("nope: {}", words::REFUSED_NOT_LISTED)),
        "{}",
        partly.stderr
    );

    let whole = render_dismiss(&DismissOutcome {
        refusal: Some(Refusal::new(RefusalReason::AgentCaller)),
        ..DismissOutcome::default()
    });
    assert_eq!(whole.code, EXIT_REFUSED);
    assert!(whole.stderr.contains(words::REFUSED_AGENT_CALLER));
}

/// A client whose socket has no daemon behind it: every call that reaches
/// it fails as unreachable, so a test can tell "refused before asking".
fn no_daemon() -> (tempfile::TempDir, DaemonClient) {
    let dir = crate::test_temp::tempdir().unwrap();
    let client = DaemonClient::new(dir.path().join("no-daemon.sock"));
    (dir, client)
}

#[test]
fn dismiss_all_without_yes_and_no_terminal_refuses_before_asking_the_daemon() {
    let (_dir, client) = no_daemon();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt.block_on(run_dismiss(
        &client,
        &DismissArgs {
            ids: Vec::new(),
            all: true,
            yes: false,
        },
        &AmbientIdentity::Person,
        Confirm::NoTerminal,
    ));
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(
        out.stderr.contains(words::DISMISS_ALL_NEEDS_YES),
        "{}",
        out.stderr
    );
}

#[test]
fn dismiss_needs_ids_or_all_but_not_both() {
    let (_dir, client) = no_daemon();
    let rt = tokio::runtime::Runtime::new().unwrap();
    for args in [
        DismissArgs::default(),
        DismissArgs {
            ids: vec!["x".into()],
            all: true,
            yes: true,
        },
    ] {
        let out = rt.block_on(run_dismiss(
            &client,
            &args,
            &AmbientIdentity::Person,
            Confirm::NoTerminal,
        ));
        assert_eq!(out.code, EXIT_REFUSED, "{args:?}");
        assert!(out.stderr.contains(words::REFUSED_BAD_REQUEST), "{args:?}");
    }
}

#[test]
fn no_daemon_is_exit_three() {
    let (_dir, client) = no_daemon();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt.block_on(run_list(&client, false, false));
    assert_eq!(out.code, EXIT_UNREACHABLE, "{out:?}");
    assert!(out.stderr.contains(words::NO_DAEMON), "{}", out.stderr);
}

/// Reviewer 5 and 3: an item prints its progress on its own line, with the
/// shared wording, and a show that carried the bodies of fewer reports than
/// the item lists says how to read the rest by id; a list that left report
/// entries out, or cut a string, says so.
#[test]
fn an_item_prints_its_progress_and_how_to_read_reports_not_shown() {
    let mut unit = item(UNIT_ID, ItemKind::Unit, "team");
    unit.progress = crate::interrupted_work::Progress {
        attempts: 3,
        roles: vec!["orchestrator".into(), "coder".into()],
        delegations: 4,
        delegations_done: 2,
    };
    for n in 1..=18 {
        unit.reports.push(ItemReport {
            report_id: format!("r-{n}"),
            task_id: None,
            unit_id: None,
            kept_until_ms: None,
        });
    }
    let text = render_item(&unit);
    assert!(
        text.contains(&format!(
            "    {}: {}\n",
            words::LABEL_PROGRESS,
            words::progress_line(&unit.progress)
        )),
        "{text}"
    );
    assert!(
        text.contains("3 attempts; roles orchestrator, coder; 2 of 4 delegations done"),
        "{text}"
    );

    let one = ShownReport {
        report_id: "r-17".into(),
        body: Some("[UNTRUSTED-WORKER-REPORT:\nseventeen\n:END-UNTRUSTED-WORKER-REPORT]\n".into()),
        cut: false,
        error: None,
    };
    let show = InterruptedShow {
        item: unit.clone(),
        reports: vec![one],
        incomplete: false,
        bodies_omitted: 0,
    };
    let text = render_show(&show);
    assert!(text.contains("seventeen"), "{text}");
    assert!(
        text.contains(&words::more_reports_sentence(17, UNIT_ID)),
        "{text}"
    );
    assert!(
        text.contains(&format!("interrupted show {UNIT_ID} --report")),
        "{text}"
    );

    let mut listed = unit.clone();
    listed.reports.truncate(16);
    listed.reports_omitted = 2;
    listed.cut = true;
    let text = render_item(&listed);
    assert!(
        text.contains(&words::more_reports_sentence(2, UNIT_ID)),
        "{text}"
    );
    assert!(text.contains(words::ITEM_CUT), "{text}");
}

/// Auditor 4: a dismissal whose report was capped says how many more items
/// were dismissed and how many more ids were refused, and a refusal it did
/// not list still fails the command.
#[test]
fn a_capped_dismissal_report_counts_what_it_left_out() {
    let outcome = DismissOutcome {
        dismissed: vec![DismissedItem {
            id: UNIT_ID.into(),
            name: None,
        }],
        refused: Vec::new(),
        refusal: None,
        incomplete: false,
        dismissed_omitted: 5,
        refused_omitted: 1,
    };
    let out = render_dismiss(&outcome);
    assert!(
        out.stdout.contains(&words::more_dismissed_sentence(5)),
        "{}",
        out.stdout
    );
    assert!(
        out.stderr.contains(&words::more_refused_sentence(1)),
        "{}",
        out.stderr
    );
    assert_eq!(out.code, EXIT_REFUSED);
}

/// Auditor N3: Unicode bidi controls in a name, branch or path read from the
/// journal (a branch name Git accepts can carry U+202E) cannot reorder what
/// the person reads: none survives into the list, the show or a reclaim's
/// answer, and the text around them still shows.
#[test]
fn bidi_controls_from_the_journal_cannot_reorder_the_text() {
    use crate::untrusted_text::is_bidi_format_char;
    let live = |text: &str| text.chars().any(is_bidi_format_char);
    let mut unit = item(UNIT_ID, ItemKind::Unit, "fix\u{202e}nigol");
    unit.branch = Some("agent/\u{202e}txt.exe".into());
    unit.worktree = Some("/work/\u{2066}a\u{2069}".into());
    unit.launcher = Some("caller\u{200f}".into());
    let text = render_item(&unit);
    assert!(!live(&text), "{text:?}");
    assert!(text.contains("txt.exe") && text.contains("nigol"), "{text}");
    let show = render_show(&InterruptedShow {
        item: unit.clone(),
        reports: Vec::new(),
        incomplete: false,
        bodies_omitted: 0,
    });
    assert!(!live(&show), "{show:?}");

    let mut reclaimed = outcome(ReclaimVerdict::Removed);
    reclaimed.worktree = unit.worktree.clone();
    reclaimed.branch = unit.branch.clone();
    reclaimed.nested_worktrees = vec!["/work/\u{202d}inner".into()];
    let out = render_reclaim(&reclaimed);
    assert!(!live(&out.stdout) && !live(&out.stderr), "{out:?}");
}
