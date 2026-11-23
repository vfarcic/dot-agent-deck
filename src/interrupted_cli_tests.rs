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
        agents: Vec::new(),
        agents_omitted: 0,
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
        reports_from: 0,
        next_reports_from: None,
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
        stopped: None,
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

/// PR #2431 round 5 (Qodo): a running daemon on another attach protocol,
/// reached through a client that checks the protocol, is a daemon that cannot
/// serve interrupted work: exit 2 with the shared sentence, and it is sent
/// nothing past its `Hello`. No daemon at all stays exit 3. (The one-shot
/// CLI's own client does not check the protocol, so it never meets this
/// error; the mapping is what any caller of these functions gets.)
#[cfg(unix)]
#[test]
fn a_daemon_on_another_protocol_is_exit_two_and_no_daemon_is_exit_three() {
    use crate::daemon_protocol::{AttachResponse, PROTOCOL_VERSION, read_frame, write_resp};
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = crate::test_temp::tempdir().unwrap();
    let path = dir.path().join("other-protocol.sock");
    let listener = {
        let _enter = rt.enter();
        tokio::net::UnixListener::bind(&path).expect("bind the fake daemon")
    };
    let after_hello = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = after_hello.clone();
    rt.spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                while let Ok(Some((_kind, payload))) = read_frame(&mut stream).await {
                    let request: serde_json::Value =
                        serde_json::from_slice(&payload).unwrap_or_default();
                    let op = request["op"].as_str().unwrap_or("").to_string();
                    let response = if op == "hello" {
                        AttachResponse {
                            capabilities: Some(vec![
                                crate::daemon_protocol::CAP_INTERRUPTED_WORK.to_string(),
                                crate::daemon_protocol::CAP_HELLO_FOLLOW_ON.to_string(),
                            ]),
                            ..AttachResponse::hello(PROTOCOL_VERSION + 1)
                        }
                    } else {
                        log.lock().unwrap().push(op);
                        AttachResponse::err("not sent by a gated client")
                    };
                    if write_resp(&mut stream, &response).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    let other = DaemonClient::new(path).with_protocol_gate();
    let out = rt.block_on(run_list(&other, false, false));
    assert_eq!(out.code, EXIT_DAEMON_TOO_OLD, "{out:?}");
    assert!(out.stderr.contains(words::UNSUPPORTED), "{}", out.stderr);
    assert!(
        after_hello.lock().unwrap().is_empty(),
        "nothing past the Hello: {:?}",
        after_hello.lock().unwrap()
    );

    let (_none, absent) = no_daemon();
    let out = rt.block_on(run_list(&absent.with_protocol_gate(), false, false));
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
        reports_from: 0,
        next_reports_from: None,
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
        stopped: None,
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
        reports_from: 0,
        next_reports_from: None,
    });
    assert!(!live(&show), "{show:?}");

    let mut reclaimed = outcome(ReclaimVerdict::Removed);
    reclaimed.worktree = unit.worktree.clone();
    reclaimed.branch = unit.branch.clone();
    reclaimed.nested_worktrees = vec!["/work/\u{202d}inner".into()];
    let out = render_reclaim(&reclaimed);
    assert!(!live(&out.stdout) && !live(&out.stderr), "{out:?}");
}

/// Reviewer 1: an item whose kept reports go past what the answer carries
/// says how to list the next ones (`--reports-from`), and a show of a page
/// says which reports it holds and where the next page starts.
#[test]
fn an_item_with_more_reports_points_at_the_next_page() {
    let mut unit = item(UNIT_ID, ItemKind::Unit, "a");
    for n in 1..=16 {
        unit.reports.push(ItemReport {
            report_id: format!("r-{n}"),
            task_id: None,
            unit_id: None,
            kept_until_ms: None,
        });
    }
    unit.reports_omitted = 24;
    let listed = render_item(&unit);
    assert!(
        listed.contains(&format!(
            "dot-agent-deck interrupted show {UNIT_ID} --reports-from 16"
        )),
        "{listed}"
    );

    let mut paged = unit.clone();
    paged.reports = (17..=32)
        .map(|n| ItemReport {
            report_id: format!("r-{n}"),
            task_id: None,
            unit_id: None,
            kept_until_ms: None,
        })
        .collect();
    let page = InterruptedShow {
        item: paged,
        reports: Vec::new(),
        incomplete: false,
        bodies_omitted: 0,
        reports_from: 16,
        next_reports_from: Some(32),
    };
    let text = render_show(&page);
    assert!(text.contains("Reports 17 to 32 of 40."), "{text}");
    assert!(
        text.contains(&format!(
            "dot-agent-deck interrupted show {UNIT_ID} --reports-from 32"
        )),
        "{text}"
    );
    assert!(text.contains("Report: r-17,"), "{text}");
}

/// Auditor N3 (second round): a malformed reply, decoded as the client
/// decodes the daemon's answer, whose report body and dismissed id carry
/// escape sequences, a carriage return and bidi controls. The person's
/// output keeps the body's own lines and tabs and the text around those
/// characters, and carries none of them live.
#[test]
fn a_malformed_reply_cannot_move_the_terminal_through_a_body_or_a_dismissed_id() {
    use crate::untrusted_text::is_bidi_format_char;
    // Live: a bidi control, or a control character other than the layout
    // a multi-line body is allowed (its newlines and tabs).
    let live = |text: &str| {
        text.chars()
            .any(|c| is_bidi_format_char(c) || (c.is_control() && c != '\n' && c != '\t'))
    };
    let mut show = serde_json::to_value(InterruptedShow {
        item: item(UNIT_ID, ItemKind::Unit, "a"),
        reports: Vec::new(),
        incomplete: false,
        bodies_omitted: 0,
        reports_from: 0,
        next_reports_from: None,
    })
    .unwrap();
    show["reports"] = serde_json::json!([{
        "report_id": "r-1",
        "body": "first\tcolumn\n\u{1b}]0;owned\u{7}title\n\u{1b}[2Jcleared\rover\u{202e}desrever\u{9b}31m\n",
    }]);
    let show: InterruptedShow = serde_json::from_value(show).unwrap();
    let text = render_show(&show);
    assert!(!live(&text), "{text:?}");
    assert!(text.contains("first\tcolumn\n"), "{text:?}");
    for visible in ["owned", "title", "cleared", "over", "desrever"] {
        assert!(text.contains(visible), "{visible}: {text:?}");
    }

    let dismissed: DismissOutcome = serde_json::from_value(serde_json::json!({
        "dismissed": [{"id": "w-\u{1b}[31mred\u{202e}di", "name": "a"}],
    }))
    .unwrap();
    let out = render_dismiss(&dismissed);
    assert!(!live(&out.stdout) && !live(&out.stderr), "{out:?}");
    assert!(out.stdout.contains("red"), "{out:?}");
}

// ---------------------------------------------------------------------------
// A dismissal pinned to one daemon process (verification finding A)
// ---------------------------------------------------------------------------

/// One request a [`replacing_daemon`] received after its `Hello`: the daemon
/// process that received it, its `op` and the ids it named.
#[cfg(unix)]
#[derive(Debug, Clone)]
struct Received {
    instance: String,
    op: String,
    ids: Vec<String>,
}

/// A fake daemon on its own socket whose `Hello` names the process
/// `first` on its first `replaced_after` connections, and `second` after
/// them, as a daemon replaced between two requests would. It lists `listed`
/// items and dismisses every id it is sent. Answers the client, and every
/// request after a `Hello` it received.
#[cfg(unix)]
fn replacing_daemon(
    listed: usize,
    replaced_after: usize,
) -> (
    tempfile::TempDir,
    DaemonClient,
    std::sync::Arc<std::sync::Mutex<Vec<Received>>>,
    tokio::runtime::Runtime,
) {
    use crate::daemon_protocol::{AttachResponse, PROTOCOL_VERSION, read_frame, write_resp};
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = crate::test_temp::tempdir().unwrap();
    let path = dir.path().join("replacing-daemon.sock");
    let listener = {
        let _enter = rt.enter();
        tokio::net::UnixListener::bind(&path).expect("bind the fake daemon")
    };
    let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Received>::new()));
    let log = received.clone();
    let ids: Vec<String> = (0..listed)
        .map(|n| format!("w-abcdefgh-{:016}", 1000 + n))
        .collect();
    rt.spawn(async move {
        let mut connection = 0usize;
        while let Ok((mut stream, _)) = listener.accept().await {
            connection += 1;
            let instance = if connection <= replaced_after {
                "first-process"
            } else {
                "second-process"
            };
            let log = log.clone();
            let ids = ids.clone();
            tokio::spawn(async move {
                let mut frames = 0usize;
                while let Ok(Some((_kind, payload))) = read_frame(&mut stream).await {
                    frames += 1;
                    let request: serde_json::Value =
                        serde_json::from_slice(&payload).unwrap_or_default();
                    let op = request["op"].as_str().unwrap_or("").to_string();
                    let response = if frames == 1 && op == "hello" {
                        AttachResponse {
                            capabilities: Some(vec![
                                crate::daemon_protocol::CAP_INTERRUPTED_WORK.to_string(),
                                crate::daemon_protocol::CAP_HELLO_FOLLOW_ON.to_string(),
                            ]),
                            instance_id: Some(instance.to_string()),
                            ..AttachResponse::hello(PROTOCOL_VERSION)
                        }
                    } else {
                        let named: Vec<String> = request["ids"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        log.lock().unwrap().push(Received {
                            instance: instance.to_string(),
                            op: op.clone(),
                            ids: named.clone(),
                        });
                        match op.as_str() {
                            "list-interrupted-work" => AttachResponse {
                                interrupted_work: Some(InterruptedList {
                                    items: ids
                                        .iter()
                                        .enumerate()
                                        .map(|(n, id)| {
                                            item(id, ItemKind::Unit, &format!("unit-{n}"))
                                        })
                                        .collect(),
                                    incomplete: false,
                                    omitted: 0,
                                }),
                                ..AttachResponse::ok()
                            },
                            "dismiss-interrupted" => AttachResponse {
                                interrupted_dismiss: Some(DismissOutcome {
                                    dismissed: named
                                        .into_iter()
                                        .map(|id| DismissedItem { id, name: None })
                                        .collect(),
                                    ..DismissOutcome::default()
                                }),
                                ..AttachResponse::ok()
                            },
                            _ => AttachResponse::err("malformed request"),
                        }
                    };
                    if write_resp(&mut stream, &response).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let client = DaemonClient::new(path);
    (dir, client, received, rt)
}

/// Scenario: `interrupted dismiss` names 513 items, more than one request
/// takes, so the CLI sends them in batches. The daemon process is replaced
/// after the first batch answered: the CLI sends the replacement no
/// dismissal, prints the items the first batch dismissed, says the rest may
/// not have been dismissed because the daemon was replaced, and exits 1.
#[cfg(unix)]
#[test]
fn a_dismissal_in_batches_sends_a_replaced_daemon_nothing() {
    let (_dir, client, received, rt) = replacing_daemon(0, 1);
    let ids: Vec<String> = (0..513)
        .map(|n| format!("w-abcdefgh-{:016}", 5000 + n))
        .collect();
    let out = rt.block_on(run_dismiss(
        &client,
        &DismissArgs {
            ids: ids.clone(),
            all: false,
            yes: false,
        },
        &AmbientIdentity::Person,
        Confirm::NoTerminal,
    ));
    let received = received.lock().unwrap().clone();
    let mutations: Vec<&Received> = received
        .iter()
        .filter(|r| r.op == "dismiss-interrupted")
        .collect();
    let seen: Vec<(&str, usize)> = mutations
        .iter()
        .map(|r| (r.instance.as_str(), r.ids.len()))
        .collect();
    assert!(
        mutations.iter().all(|r| r.instance == "first-process"),
        "no dismissal reached the replacement: {seen:?}"
    );
    assert_eq!(mutations.len(), 1, "{seen:?}");
    assert_eq!(out.code, EXIT_REFUSED, "{out:?}");
    assert!(out.stdout.contains(&ids[0]), "{}", out.stdout);
    let why = words::dismiss_stopped_sentence(&words::DismissStopped {
        reason: words::DismissStopReason::Replaced,
        refusal: None,
        not_sent: 513 - mutations[0].ids.len(),
    });
    assert!(out.stderr.contains(&why), "{why:?} in {}", out.stderr);
}

/// Scenario: `interrupted dismiss --all` lists the items and asks; while the
/// question is open, the daemon process is replaced. After `y`, the CLI sends
/// the replacement no dismissal, says nothing was dismissed because the
/// daemon was replaced, and exits 1.
#[cfg(unix)]
#[test]
fn a_dismiss_all_confirmed_against_a_replaced_daemon_sends_it_nothing() {
    let (_dir, client, received, rt) = replacing_daemon(3, 1);
    let mut asked = |_: &str| true;
    let out = rt.block_on(run_dismiss(
        &client,
        &DismissArgs {
            ids: Vec::new(),
            all: true,
            yes: false,
        },
        &AmbientIdentity::Person,
        Confirm::Ask(&mut asked),
    ));
    let received = received.lock().unwrap().clone();
    let seen: Vec<(&str, &str, usize)> = received
        .iter()
        .map(|r| (r.instance.as_str(), r.op.as_str(), r.ids.len()))
        .collect();
    assert!(
        !received
            .iter()
            .any(|r| r.op == "dismiss-interrupted" && r.instance != "first-process"),
        "no dismissal reached the replacement: {seen:?}"
    );
    assert_eq!(out.code, EXIT_REFUSED, "{out:?}");
    assert!(
        out.stderr.contains(words::DISMISS_STOPPED_REPLACED),
        "{}",
        out.stderr
    );
    assert!(
        !out.stdout.contains(words::NOTHING_INTERRUPTED),
        "{}",
        out.stdout
    );
}

/// Docs review R2: a dismissal the daemon refused whole because its journal
/// read was incomplete says so and exits 1, dismissing nothing.
#[test]
fn a_dismissal_refused_on_an_incomplete_read_says_so() {
    let out = render_dismiss(&DismissOutcome {
        refusal: Some(Refusal::new(RefusalReason::IncompleteRead)),
        ..DismissOutcome::default()
    });
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(
        out.stderr.contains(words::REFUSED_INCOMPLETE_READ),
        "{}",
        out.stderr
    );
}

// ---------------------------------------------------------------------------
// PRD #1790 stage 3: restore and start-over
// ---------------------------------------------------------------------------

const ATTEMPT: &str = "r-restorecli/7";

fn restored(first_message_posted: bool, recorded: bool) -> words::RestoredAgent {
    words::RestoredAgent {
        attempt: crate::work_journal::AttemptId::parse(ATTEMPT).unwrap(),
        agent_id: "3".into(),
        pane_id: "sched-restored-fix-login-abcdef-0".into(),
        name: Some("fix-login".into()),
        first_message_posted,
        recorded,
    }
}

fn refused_attempt(reason: words::RestoreRefusal) -> words::RestoreRefused {
    words::RestoreRefused::new(crate::work_journal::AttemptId::parse(ATTEMPT), reason)
}

#[test]
fn show_prints_each_agent_its_attempt_and_its_verdict_with_the_flag_on() {
    crate::features::set_for_test(crate::features::Features::test_with(true));
    let mut unit = item(UNIT_ID, ItemKind::Unit, "fix-login");
    unit.agents.push(words::ItemAgent {
        attempt: crate::work_journal::AttemptId::parse(ATTEMPT).unwrap(),
        name: Some("fix-login".into()),
        agent_type: Some(crate::event::AgentType::ClaudeCode),
        role: None,
        folder: Some("/work/fix-login".into()),
        restore: words::RestoreVerdict::Resume,
    });
    unit.agents.push(words::ItemAgent {
        attempt: crate::work_journal::AttemptId::parse("r-restorecli/8").unwrap(),
        name: Some("other".into()),
        agent_type: Some(crate::event::AgentType::Pi),
        role: None,
        folder: Some("/work/fix-login".into()),
        restore: words::RestoreVerdict::StartOver {
            reason: words::RestoreReason::NotResumed,
        },
    });
    let text = render_item(&unit);
    assert!(text.contains(ATTEMPT), "{text}");
    assert!(text.contains(words::RESTORE_VERDICT_RESUME), "{text}");
    assert!(text.contains(words::START_OVER), "{text}");
    crate::features::set_for_test(crate::features::Features::test_with(false));
    let text = render_item(&unit);
    assert!(
        !text.contains(ATTEMPT) && !text.contains(words::RESTORE_VERDICT_RESUME),
        "no verdicts with the flag off: {text}"
    );
}

#[test]
fn restore_and_start_over_are_refused_before_anything_is_sent_when_off_or_unconfirmed() {
    let (_dir, client) = no_daemon();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let restore = |dry_run: bool, yes: bool| RestoreArgs {
        id: UNIT_ID.into(),
        agents: Vec::new(),
        mode: words::RestoreMode::Continue,
        dry_run,
        yes,
    };
    // The flag off: refused, naming it, whatever was confirmed.
    crate::features::set_for_test(crate::features::Features::test_with(false));
    let out = rt.block_on(run_restore(
        &client,
        &restore(false, true),
        &AmbientIdentity::Person,
        Confirm::NoTerminal,
    ));
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(out.stderr.contains("experimental"), "{}", out.stderr);
    let out = rt.block_on(run_start_over(
        &client,
        &StartOverArgs {
            id: UNIT_ID.into(),
            agent: ATTEMPT.into(),
            dry_run: false,
            yes: true,
        },
        &AmbientIdentity::Person,
        Confirm::NoTerminal,
    ));
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(out.stderr.contains("experimental"), "{}", out.stderr);

    // On: no terminal and no --yes refuses and names --yes; a person who
    // says no restores nothing. Neither reaches the (absent) daemon, which
    // would be exit 3.
    crate::features::set_for_test(crate::features::Features::test_with(true));
    let out = rt.block_on(run_restore(
        &client,
        &restore(false, false),
        &AmbientIdentity::Person,
        Confirm::NoTerminal,
    ));
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(out.stderr.contains("--yes"), "{}", out.stderr);
    let mut asked = Vec::new();
    let mut no = |question: &str| {
        asked.push(question.to_string());
        false
    };
    let out = rt.block_on(run_restore(
        &client,
        &restore(false, false),
        &AmbientIdentity::Person,
        Confirm::Ask(&mut no),
    ));
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(
        out.stderr.contains(words::NOTHING_RESTORED),
        "{}",
        out.stderr
    );
    assert_eq!(asked.len(), 1);
    assert!(asked[0].starts_with(&words::restore_question(UNIT_ID)));
    // A dry run needs no confirmation and is sent: no daemon is exit 3.
    let out = rt.block_on(run_restore(
        &client,
        &restore(true, false),
        &AmbientIdentity::Person,
        Confirm::NoTerminal,
    ));
    assert_eq!(out.code, EXIT_UNREACHABLE, "{}", out.stderr);
    crate::features::set_for_test(crate::features::Features::test_with(false));
}

#[test]
fn a_restore_exits_zero_only_when_something_came_back_and_nothing_was_refused() {
    let done = words::RestoreOutcome {
        restored: vec![restored(true, true)],
        ..Default::default()
    };
    let out = render_restore(&done);
    assert_eq!(out.code, EXIT_OK);
    assert!(
        out.stdout.contains(&words::restored_sentence("fix-login")),
        "{}",
        out.stdout
    );
    assert!(out.stderr.is_empty(), "{}", out.stderr);

    let quiet = words::RestoreOutcome {
        restored: vec![restored(false, false)],
        ..Default::default()
    };
    let out = render_restore(&quiet);
    assert_eq!(out.code, EXIT_OK);
    assert!(out.stderr.contains(words::RESTART_NOTE_NOT_SENT));
    assert!(out.stderr.contains(words::RESTORE_NOT_RECORDED));

    let refused = words::RestoreOutcome {
        refused: vec![refused_attempt(words::RestoreRefusal::NotResumed)],
        ..Default::default()
    };
    let out = render_restore(&refused);
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(out.stderr.contains(ATTEMPT), "{}", out.stderr);
    assert!(
        out.stderr.contains(words::restore_refusal_sentence(
            words::RestoreRefusal::NotResumed
        )),
        "{}",
        out.stderr
    );

    let would = words::RestoreOutcome {
        dry_run: true,
        would_restore: vec![crate::work_journal::AttemptId::parse(ATTEMPT).unwrap()],
        ..Default::default()
    };
    assert_eq!(render_restore(&would).code, EXIT_OK);
    assert_eq!(
        render_restore(&words::RestoreOutcome::default()).code,
        EXIT_REFUSED
    );

    // A newer daemon's refusal shows its own sentence, escaped.
    let mut unknown = refused_attempt(words::RestoreRefusal::Unknown);
    unknown.message = "newer\x1b[2Jreason".into();
    let out = render_restore(&words::RestoreOutcome {
        refused: vec![unknown],
        ..Default::default()
    });
    assert!(!out.stderr.contains('\x1b'), "{:?}", out.stderr);
    assert!(out.stderr.contains("newer"), "{}", out.stderr);
}

#[test]
fn a_start_over_says_what_it_started_or_why_it_did_not() {
    let out = render_start_over(&words::StartOverOutcome {
        started: Some(restored(true, true)),
        ..Default::default()
    });
    assert_eq!(out.code, EXIT_OK);
    assert!(
        out.stdout
            .contains(&words::started_over_sentence("fix-login"))
    );

    let out = render_start_over(&words::StartOverOutcome {
        started: Some(restored(false, true)),
        ..Default::default()
    });
    assert_eq!(out.code, EXIT_OK);
    assert!(out.stderr.contains(words::SAVED_TASK_NOT_SENT));

    let out = render_start_over(&words::StartOverOutcome {
        refused: Some(refused_attempt(words::RestoreRefusal::TaskUnsafe)),
        ..Default::default()
    });
    assert_eq!(out.code, EXIT_REFUSED);
    assert!(out.stderr.contains(words::restore_refusal_sentence(
        words::RestoreRefusal::TaskUnsafe
    )));

    let out = render_start_over(&words::StartOverOutcome {
        dry_run: true,
        would_start: true,
        ..Default::default()
    });
    assert_eq!(out.code, EXIT_OK);
    assert!(out.stdout.contains(words::START_OVER_WOULD));
}
