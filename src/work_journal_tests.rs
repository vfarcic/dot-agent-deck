//! PRD #1889: the work journal's unit tests. Every test works in its own
//! disk-backed temp directory; none sleeps or reaches the network.

use super::*;

use std::collections::BTreeSet;
use std::fs;

const DECK: &str = "abcdefgh";
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// A fixed "now" for compaction, well past every planted timestamp.
const NOW_MS: u64 = 1_800_000_000_000;

fn deck() -> DeckId {
    DeckId::parse(DECK).unwrap()
}

fn work(n: u64) -> WorkId {
    WorkId::parse(&format!("w-{DECK}-{n:016x}")).unwrap()
}

fn attempt(run: &str, agent: u64) -> AttemptId {
    AttemptId::new(run, agent).unwrap()
}

fn home() -> PathBuf {
    PathBuf::from("/home/someone")
}

fn options() -> JournalOptions {
    JournalOptions {
        home: home(),
        ..JournalOptions::default()
    }
}

fn state() -> tempfile::TempDir {
    crate::test_temp::tempdir().unwrap()
}

fn open(state: &Path, run: &str) -> WorkJournal {
    WorkJournal::try_open_with(state, run, &deck(), options()).unwrap()
}

fn journal_dir(state: &Path) -> PathBuf {
    state.join(DIR_NAME)
}

/// Make the journal directory as the daemon would, owner-only.
fn make_dir(state: &Path) -> PathBuf {
    let dir = journal_dir(state);
    fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

fn line(run: &str, seq: u64, ts_ms: u64, event: Event) -> Line {
    Line {
        v: SCHEMA_VERSION,
        seq,
        ts_ms,
        run: run.to_string(),
        event,
    }
}

fn encoded(line: &Line) -> String {
    String::from_utf8(encode_line_with_home(line, &home()).unwrap()).unwrap()
}

/// Write a dead run's segment (no lock file at all) from raw lines.
fn plant(state: &Path, name: &str, lines: &[String], tail: &str) -> PathBuf {
    let path = make_dir(state).join(name);
    let mut text = String::new();
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    text.push_str(tail);
    fs::write(&path, text).unwrap();
    path
}

fn opened(w: WorkId, parent: Option<WorkId>) -> Event {
    Event::Opened {
        work: w,
        launch: Launch::Pane,
        parent,
        refs: Box::new(Refs {
            pane: Some("3".into()),
            ..Refs::default()
        }),
        workspace: Some(
            Workspace::from_path_with_home(Path::new("/home/someone/code/x"), &home()).unwrap(),
        ),
    }
}

fn started(w: WorkId, run: &str, agent: u64) -> Event {
    Event::AttemptStarted {
        work: w,
        attempt: attempt(run, agent),
        pane: "3".into(),
        agent,
        role: Some("coder".into()),
    }
}

fn ended(w: WorkId, run: &str, agent: u64) -> Event {
    Event::AttemptEnded {
        work: w,
        attempt: attempt(run, agent),
        how: EndHow::Exited,
        code: Some(0),
        cause: None,
    }
}

fn closed(w: WorkId) -> Event {
    Event::Closed {
        work: w,
        how: CloseHow::PaneClosed,
    }
}

fn segment_lines(state: &Path, run: &str) -> Vec<Line> {
    let text = fs::read_to_string(journal_dir(state).join(segment_name(run))).unwrap();
    text.lines()
        .map(|l| match decode_line(l.as_bytes()) {
            Decoded::Event(line) => line,
            other => panic!("own segment holds an unreadable line {l:?}: {other:?}"),
        })
        .collect()
}

fn interrupted_attempts(lines: &[Line]) -> Vec<(String, WorkId)> {
    lines
        .iter()
        .filter_map(|l| match &l.event {
            Event::AttemptEnded {
                work,
                attempt,
                how: EndHow::Interrupted,
                cause: Some(EndCause::DaemonGone),
                code: None,
            } => Some((attempt.to_string(), work.clone())),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Ids and codec
// ---------------------------------------------------------------------------

#[test]
fn work_id_has_the_contract_format() {
    let a = WorkId::mint(&deck()).unwrap();
    let b = WorkId::mint(&deck()).unwrap();
    assert_ne!(a, b, "two mints must differ");
    let text = a.as_str();
    assert_eq!(text.len(), 2 + 8 + 1 + 16, "{text}");
    let hex = text.strip_prefix(&format!("w-{DECK}-")).expect(text);
    assert!(
        hex.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{text}"
    );
    assert_eq!(WorkId::parse(text), Some(a.clone()));

    for bad in [
        "",
        "w-abcdefgh-0123456789abcde",   // 15 hex
        "w-abcdefgh-0123456789abcdef0", // 17 hex
        "w-abcdefgh-0123456789ABCDEF",  // upper case
        "w-ABCDEFGH-0123456789abcdef",  // bad deck
        "w-abcdefg-0123456789abcdef",   // short deck
        "m-abcdefgh-0123456789abcdef",  // a message id
        "w-abcdefgh_0123456789abcdef",
    ] {
        assert_eq!(WorkId::parse(bad), None, "{bad:?} parsed");
    }
}

#[test]
fn attempt_id_is_run_slash_agent() {
    let a = attempt("0123abcd", 42);
    assert_eq!(a.as_str(), "0123abcd/42");
    assert_eq!(a.run(), "0123abcd");
    assert_eq!(AttemptId::parse("0123abcd/42"), Some(a));
    for bad in [
        "0123abcd/",
        "/42",
        "0123abcd/042",
        "a b/1",
        "0123abcd/-1",
        "x/1/",
    ] {
        assert_eq!(AttemptId::parse(bad), None, "{bad:?} parsed");
    }
    assert_eq!(AttemptId::new("not/plain", 1), None);
}

#[test]
fn codec_round_trips_every_kind() {
    let run = "0123abcd";
    let events = vec![
        Event::Opened {
            work: work(1),
            launch: Launch::Dispatch,
            parent: Some(work(2)),
            refs: Box::new(Refs {
                issue: Some(1889),
                repo: Some("owner/name".into()),
                branch: Some("agent/dispatch-x".into()),
                pr: Some(7),
                task_id: Some("m-abcdefgh-0123456789abcdef".into()),
                unit_id: Some("u-1-2".into()),
                orchestration_id: Some("orch-0123456789abcdef-1".into()),
                orchestration: Some("tdd".into()),
                role: Some("coder".into()),
                schedule: Some("nightly".into()),
                pane: Some("desktop-0123456789abcdef-3".into()),
            }),
            workspace: Some(
                Workspace::from_path_with_home(Path::new("/home/someone/code/é"), &home()).unwrap(),
            ),
        },
        started(work(1), run, 9),
        ended(work(1), run, 9),
        Event::AttemptEnded {
            work: work(1),
            attempt: attempt(run, 10),
            how: EndHow::Interrupted,
            code: None,
            cause: Some(EndCause::DaemonShutdown),
        },
        Event::Linked {
            work: work(1),
            refs: Box::new(Refs {
                pr: Some(12),
                ..Refs::default()
            }),
        },
        Event::Reported {
            work: work(3),
            report_id: "r-0123".into(),
            task_id: Some("m-abcdefgh-0123456789abcdef".into()),
            unit_id: None,
            destination: Destination::Recovery,
            to: Some(work(2)),
        },
        closed(work(1)),
    ];
    for (i, event) in events.into_iter().enumerate() {
        let l = line(run, i as u64 + 1, 1_700_000_000_000 + i as u64, event);
        let bytes = encode_line_with_home(&l, &home()).unwrap();
        assert!(bytes.is_ascii(), "line is not ASCII: {bytes:?}");
        assert!(!bytes.contains(&b'\n'));
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            text.starts_with(&format!(
                "{{\"v\":1,\"seq\":{},\"ts_ms\":{},\"run\":\"{run}\",\"kind\":\"{}\",\"work\":\"",
                l.seq,
                l.ts_ms,
                l.event.kind()
            )),
            "header order: {text}"
        );
        assert_eq!(decode_line(&bytes), Decoded::Event(l));
    }
}

#[test]
fn reader_skips_a_newer_version_and_an_unknown_kind() {
    let v2 = format!(
        r#"{{"v":2,"seq":1,"ts_ms":1,"run":"r1","kind":"opened","work":"{}","launch":"pane"}}"#,
        work(1)
    );
    let unknown = format!(
        r#"{{"v":1,"seq":2,"ts_ms":2,"run":"r1","kind":"teleported","work":"{}"}}"#,
        work(1)
    );
    assert_eq!(decode_line(v2.as_bytes()), Decoded::NewerVersion);
    assert_eq!(decode_line(unknown.as_bytes()), Decoded::UnknownKind);

    let state = state();
    let good = encoded(&line("r1", 3, 3, opened(work(1), None)));
    plant(state.path(), "run-r1.jsonl", &[v2, unknown, good], "");
    let read = read_journal(state.path()).unwrap();
    assert_eq!(read.lines.len(), 1);
    assert_eq!(read.lines[0].seq, 3);
    assert_eq!(read.stats.newer_version, 1);
    assert_eq!(read.stats.unknown_kind, 1);
}

#[test]
fn reader_ignores_unknown_fields() {
    let text = format!(
        r#"{{"v":1,"seq":1,"ts_ms":5,"run":"r1","kind":"opened","work":"{}","launch":"pane","future":{{"x":[1,2]}},"refs":{{"pane":"3","later":"y"}}}}"#,
        work(1)
    );
    match decode_line(text.as_bytes()) {
        Decoded::Event(l) => {
            assert_eq!(l.event.work(), &work(1));
            match l.event {
                Event::Opened { refs, launch, .. } => {
                    assert_eq!(launch, Launch::Pane);
                    assert_eq!(refs.pane.as_deref(), Some("3"));
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("not read: {other:?}"),
    }
}

#[test]
fn reader_drops_a_torn_last_line() {
    let state = state();
    let a = encoded(&line("r1", 1, 1, opened(work(1), None)));
    let b = encoded(&line("r1", 2, 2, started(work(1), "r1", 1)));
    // A whole, parsable line that never got its newline is still torn.
    plant(state.path(), "run-r1.jsonl", &[a], &b);
    let read = read_journal(state.path()).unwrap();
    assert_eq!(read.lines.len(), 1);
    assert_eq!(read.stats.torn_tails, 1);
}

#[test]
fn reader_skips_an_unparsable_middle_line() {
    let state = state();
    let a = encoded(&line("r1", 1, 1, opened(work(1), None)));
    let b = encoded(&line("r1", 2, 2, started(work(1), "r1", 1)));
    plant(
        state.path(),
        "run-r1.jsonl",
        &[a, "{\"v\":1,\"seq\":".into(), "not json".into(), b],
        "",
    );
    let read = read_journal(state.path()).unwrap();
    assert_eq!(
        read.lines.iter().map(|l| l.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(read.stats.unparsable, 2);
}

#[test]
fn reader_skips_an_over_long_line_and_reads_on() {
    let state = state();
    let a = encoded(&line("r1", 1, 1, opened(work(1), None)));
    let b = encoded(&line("r1", 2, 2, started(work(1), "r1", 1)));
    plant(
        state.path(),
        "run-r1.jsonl",
        &[a, "x".repeat(MAX_LINE_BYTES * 3), b],
        "",
    );
    let read = read_journal(state.path()).unwrap();
    assert_eq!(read.lines.len(), 2);
    assert_eq!(read.stats.too_long, 1);
}

#[test]
fn reader_merges_files_in_time_order_and_drops_duplicates() {
    let state = state();
    let a1 = encoded(&line("ra", 1, 10, opened(work(1), None)));
    let a2 = encoded(&line("ra", 2, 30, closed(work(1))));
    let b1 = encoded(&line("rb", 1, 20, opened(work(2), None)));
    plant(state.path(), "run-ra.jsonl", &[a1.clone(), a2], "");
    plant(state.path(), "run-rb.jsonl", &[b1], "");
    // A compaction that died before removing its source repeats a line.
    plant(state.path(), "compacted-rc.jsonl", &[a1], "");
    let read = read_journal(state.path()).unwrap();
    let order: Vec<(String, u64)> = read.lines.iter().map(|l| (l.run.clone(), l.seq)).collect();
    assert_eq!(
        order,
        vec![("ra".into(), 1), ("rb".into(), 1), ("ra".into(), 2)]
    );
    assert_eq!(read.stats.duplicates, 1);
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

fn started_with_role(role: &str) -> Line {
    line(
        "r1",
        1,
        1,
        Event::AttemptStarted {
            work: work(1),
            attempt: attempt("r1", 1),
            pane: "3".into(),
            agent: 1,
            role: Some(role.into()),
        },
    )
}

#[test]
fn redaction_refuses_control_characters() {
    for bad in ["a\nb", "a\rb", "a\tb", "a\u{7f}b", "a\u{85}b", "\u{0}"] {
        assert_eq!(
            encode_line_with_home(&started_with_role(bad), &home()),
            Err(Refusal::ControlCharacter { field: "role" }),
            "{bad:?}"
        );
    }
    let refs = Refs {
        branch: Some("agent/x\n".into()),
        ..Refs::default()
    };
    let l = line(
        "r1",
        1,
        1,
        Event::Linked {
            work: work(1),
            refs: Box::new(refs),
        },
    );
    assert_eq!(
        encode_line_with_home(&l, &home()),
        Err(Refusal::ControlCharacter {
            field: "refs.branch"
        })
    );
}

#[test]
fn redaction_refuses_a_string_over_1024_bytes() {
    let ok = "r".repeat(MAX_STRING_BYTES);
    assert!(encode_line_with_home(&started_with_role(&ok), &home()).is_ok());
    let long = "r".repeat(MAX_STRING_BYTES + 1);
    assert_eq!(
        encode_line_with_home(&started_with_role(&long), &home()),
        Err(Refusal::StringTooLong {
            field: "role",
            bytes: MAX_STRING_BYTES + 1
        })
    );
}

#[test]
fn workspace_writes_the_home_prefix_as_tilde() {
    let home = home();
    let ws = |p: &str| {
        Workspace::from_path_with_home(Path::new(p), &home)
            .unwrap()
            .as_str()
            .to_string()
    };
    assert_eq!(ws("/home/someone/code/x"), "~/code/x");
    assert_eq!(ws("/home/someone"), "~");
    // Component-wise: a sibling whose name merely starts with it is not home.
    assert_eq!(ws("/home/someone-else/x"), "/home/someone-else/x");
    assert_eq!(ws("/opt/work"), "/opt/work");
    // `/` as home (HOME unset) redacts nothing.
    assert_eq!(
        Workspace::from_path_with_home(Path::new("/opt/work"), Path::new("/"))
            .unwrap()
            .as_str(),
        "/opt/work"
    );

    let journal_line = line("r1", 1, 1, opened(work(1), None));
    let text = encoded(&journal_line);
    assert!(text.contains(r#""workspace":"~/code/x""#), "{text}");
    assert!(!text.contains("/home/someone"), "{text}");
}

#[test]
fn redaction_refuses_a_path_outside_workspace() {
    for bad in [
        "/home/someone/x",
        "~/x",
        "C:\\Users\\x",
        "\\\\server\\share",
    ] {
        assert_eq!(
            encode_line_with_home(&started_with_role(bad), &home()),
            Err(Refusal::PathOutsideWorkspace { field: "role" }),
            "{bad:?}"
        );
    }
    // A string carrying the home directory anywhere is refused too.
    let l = line(
        "r1",
        1,
        1,
        Event::Reported {
            work: work(1),
            report_id: "r-1".into(),
            task_id: Some("x/home/someone/y".into()),
            unit_id: None,
            destination: Destination::Inbox,
            to: None,
        },
    );
    assert_eq!(
        encode_line_with_home(&l, &home()),
        Err(Refusal::PathOutsideWorkspace { field: "task_id" })
    );
    // A branch and a repo hold slashes without being paths.
    let refs = Refs {
        branch: Some("agent/dispatch-x".into()),
        repo: Some("owner/name".into()),
        ..Refs::default()
    };
    let l = line(
        "r1",
        1,
        1,
        Event::Linked {
            work: work(1),
            refs: Box::new(refs),
        },
    );
    assert!(encode_line_with_home(&l, &home()).is_ok());
}

#[test]
fn a_line_over_4096_bytes_is_refused() {
    let big = "b".repeat(MAX_STRING_BYTES);
    let refs = Refs {
        repo: Some(big.clone()),
        branch: Some(big.clone()),
        task_id: Some(big.clone()),
        unit_id: Some(big.clone()),
        ..Refs::default()
    };
    let l = line(
        "r1",
        1,
        1,
        Event::Linked {
            work: work(1),
            refs: Box::new(refs.clone()),
        },
    );
    match encode_line_with_home(&l, &home()) {
        Err(Refusal::LineTooLong { bytes }) => assert!(bytes > MAX_LINE_BYTES),
        other => panic!("not refused: {other:?}"),
    }

    // Through the handle: nothing is written and no seq is spent.
    let state = state();
    let journal = open(state.path(), "r1");
    assert!(matches!(
        journal.try_record(Event::Linked {
            work: work(1),
            refs: Box::new(refs),
        }),
        Err(RecordError::Refused(Refusal::LineTooLong { .. }))
    ));
    assert_eq!(journal.try_record(closed(work(1))).unwrap(), 1);
    assert_eq!(segment_lines(state.path(), "r1").len(), 1);
}

#[test]
fn inconsistent_end_fields_are_refused() {
    let l = line(
        "r1",
        1,
        1,
        Event::AttemptEnded {
            work: work(1),
            attempt: attempt("r1", 1),
            how: EndHow::Closed,
            code: Some(1),
            cause: None,
        },
    );
    assert_eq!(
        encode_line_with_home(&l, &home()),
        Err(Refusal::Inconsistent { field: "code" })
    );
}

// ---------------------------------------------------------------------------
// The writer
// ---------------------------------------------------------------------------

#[test]
fn writer_appends_lines_with_seq_from_one_under_its_run_lock() {
    let state = state();
    let journal = open(state.path(), "r1");
    assert!(journal.is_enabled());
    journal.record(opened(work(1), None));
    journal.record(started(work(1), "r1", 7));
    journal.flush();
    let lines = segment_lines(state.path(), "r1");
    assert_eq!(lines.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![1, 2]);
    assert!(lines.iter().all(|l| l.run == "r1" && l.v == 1));
    let dir = journal_dir(state.path());
    assert!(dir.join("run-r1.lock").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "journal dir mode {mode:o}");
    }
    // The run is held: a second open of the same run is refused.
    assert!(WorkJournal::try_open_with(state.path(), "r1", &deck(), options()).is_err());
}

#[test]
fn a_journal_that_cannot_open_is_disabled_not_fatal() {
    let state = state();
    // The journal's directory name is taken by a file.
    fs::write(state.path().join(DIR_NAME), "x").unwrap();
    let journal = WorkJournal::open(state.path(), "r1", &deck());
    assert!(!journal.is_enabled());
    assert!(journal.disabled_reason().is_some());
    journal.record(closed(work(1)));
    assert!(matches!(
        journal.try_record(closed(work(1))),
        Err(RecordError::Disabled)
    ));
    assert_eq!(journal.recover(), RecoveryReport::default());
    assert_eq!(journal.compact(), CompactReport::default());
    // Work ids are still minted.
    let id = journal.mint_work_id().unwrap();
    assert!(id.as_str().starts_with(&format!("w-{DECK}-")));
}

#[test]
fn the_handle_is_cheap_to_share_across_threads() {
    fn assert_send_sync<T: Send + Sync + Clone>() {}
    assert_send_sync::<WorkJournal>();
    let state = state();
    let journal = open(state.path(), "r1");
    let handles: Vec<_> = (0..4)
        .map(|t| {
            let journal = journal.clone();
            std::thread::spawn(move || {
                for i in 0..10 {
                    journal.record(started(work(t), "r1", t * 100 + i));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    journal.flush();
    let seqs: Vec<u64> = segment_lines(state.path(), "r1")
        .iter()
        .map(|l| l.seq)
        .collect();
    assert_eq!(seqs, (1..=40).collect::<Vec<_>>());
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

#[test]
fn recovery_marks_only_unended_attempts() {
    let state = state();
    let dead = vec![
        encoded(&line("dead", 1, 1, opened(work(1), None))),
        encoded(&line("dead", 2, 2, started(work(1), "dead", 1))),
        encoded(&line("dead", 3, 3, ended(work(1), "dead", 1))),
        encoded(&line("dead", 4, 4, started(work(1), "dead", 2))),
        encoded(&line("dead", 5, 5, opened(work(2), Some(work(1))))),
        encoded(&line("dead", 6, 6, started(work(2), "dead", 3))),
    ];
    let path = plant(state.path(), "run-dead.jsonl", &dead, "{\"v\":1,\"se");
    let before = fs::read(&path).unwrap();

    let journal = open(state.path(), "new");
    let report = journal.try_recover().unwrap();
    assert_eq!(report.interrupted, 2);
    let own = segment_lines(state.path(), "new");
    assert_eq!(
        interrupted_attempts(&own),
        vec![
            ("dead/2".to_string(), work(1)),
            ("dead/3".to_string(), work(2))
        ]
    );
    assert!(own.iter().all(|l| l.run == "new"));
    assert_eq!(fs::read(&path).unwrap(), before, "the dead segment changed");
}

#[test]
fn recovery_is_idempotent_across_two_restarts() {
    let state = state();
    plant(
        state.path(),
        "run-dead.jsonl",
        &[
            encoded(&line("dead", 1, 1, opened(work(1), None))),
            encoded(&line("dead", 2, 2, started(work(1), "dead", 1))),
        ],
        "",
    );
    let first = open(state.path(), "first");
    assert_eq!(first.try_recover().unwrap().interrupted, 1);
    // Recovering again in the same run finds nothing new.
    assert_eq!(first.try_recover().unwrap().interrupted, 0);
    // The first restart's own attempt is live when it dies.
    first.record(opened(work(2), None));
    first.record(started(work(2), "first", 5));
    drop(first);

    let second = open(state.path(), "second");
    assert_eq!(second.try_recover().unwrap().interrupted, 1);
    assert_eq!(
        interrupted_attempts(&segment_lines(state.path(), "second")),
        vec![("first/5".to_string(), work(2))]
    );
    // Across every file each attempt is interrupted exactly once.
    let read = read_journal(state.path()).unwrap();
    let mut all = interrupted_attempts(&read.lines);
    all.sort();
    assert_eq!(
        all,
        vec![
            ("dead/1".to_string(), work(1)),
            ("first/5".to_string(), work(2))
        ]
    );
}

#[test]
fn recovery_leaves_a_live_run_alone() {
    let state = state();
    let a = open(state.path(), "live-a");
    a.record(opened(work(1), None));
    a.record(started(work(1), "live-a", 1));
    a.flush();
    let a_path = journal_dir(state.path()).join(segment_name("live-a"));
    let before = fs::read(&a_path).unwrap();

    let b = open(state.path(), "live-b");
    let report = b.try_recover().unwrap();
    assert_eq!(report.interrupted, 0);
    assert_eq!(report.live_runs, 1);
    assert!(segment_lines(state.path(), "live-b").is_empty());
    assert_eq!(fs::read(&a_path).unwrap(), before);

    // Compaction leaves it alone too.
    let compacted = b.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(compacted.consumed_files, 0);
    assert_eq!(fs::read(&a_path).unwrap(), before);
    drop(a);
}

// ---------------------------------------------------------------------------
// Compaction and retention
// ---------------------------------------------------------------------------

#[test]
fn compaction_keeps_open_works_and_ancestors_and_carries_unreadable_lines() {
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    let v2 = format!(
        r#"{{"v":2,"seq":90,"ts_ms":1,"run":"dead","kind":"opened","work":"{}"}}"#,
        work(9)
    );
    let unknown = format!(
        r#"{{"v":1,"seq":91,"ts_ms":1,"run":"dead","kind":"teleported","work":"{}"}}"#,
        work(9)
    );
    let garbage = "not json at all".to_string();
    let lines = vec![
        // The parent: finished long ago, kept because its child is open.
        encoded(&line("dead", 1, old, opened(work(1), None))),
        encoded(&line("dead", 2, old, closed(work(1)))),
        // The child: open (its attempt never ended).
        encoded(&line("dead", 3, old, opened(work(2), Some(work(1))))),
        encoded(&line("dead", 8, old, started(work(2), "dead", 2))),
        // An unrelated work: finished long ago, dropped.
        encoded(&line("dead", 4, old, opened(work(3), None))),
        encoded(&line("dead", 5, old, started(work(3), "dead", 1))),
        encoded(&line("dead", 6, old, ended(work(3), "dead", 1))),
        encoded(&line("dead", 7, old, closed(work(3)))),
        v2.clone(),
        unknown.clone(),
        garbage.clone(),
    ];
    let dead = plant(state.path(), "run-dead.jsonl", &lines, "{\"v\":1,\"torn");

    let journal = open(state.path(), "me");
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(report.consumed_files, 1);
    assert_eq!(report.dropped_works, 1);
    assert_eq!(report.dropped_lines, 4);
    assert_eq!(report.carried_unreadable, 3);
    assert!(!dead.exists(), "the folded segment is still there");

    let text = fs::read_to_string(journal_dir(state.path()).join(compacted_name("me"))).unwrap();
    let out: Vec<&str> = text.lines().collect();
    assert_eq!(
        out,
        vec![
            lines[0].as_str(),
            lines[1].as_str(),
            lines[2].as_str(),
            lines[3].as_str(),
            v2.as_str(),
            unknown.as_str(),
            garbage.as_str()
        ]
    );
    assert!(text.ends_with('\n'));
    assert!(!text.contains("torn"));

    // Compacting again changes nothing that was kept.
    journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(
        fs::read_to_string(journal_dir(state.path()).join(compacted_name("me"))).unwrap(),
        text
    );
}

#[test]
fn compaction_keeps_a_closed_work_whose_attempt_never_ended() {
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    plant(
        state.path(),
        "run-dead.jsonl",
        &[
            encoded(&line("dead", 1, old, opened(work(1), None))),
            encoded(&line("dead", 2, old, started(work(1), "dead", 1))),
            encoded(&line("dead", 3, old, closed(work(1)))),
        ],
        "",
    );
    let journal = open(state.path(), "me");
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(report.dropped_works, 0);
    assert_eq!(report.kept_lines, 3);
}

#[test]
fn compaction_drops_an_old_unclosed_work_whose_attempts_all_ended() {
    // The amended rule: "finished" does not need `closed`. A pane work whose
    // daemon went away is never closed, but recovery ends its attempt.
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    let recent = NOW_MS - DAY_MS;
    let interrupted = Event::AttemptEnded {
        work: work(1),
        attempt: attempt("dead", 1),
        how: EndHow::Interrupted,
        code: None,
        cause: Some(EndCause::DaemonGone),
    };
    plant(
        state.path(),
        "run-dead.jsonl",
        &[
            encoded(&line("dead", 1, old, opened(work(1), None))),
            encoded(&line("dead", 2, old, started(work(1), "dead", 1))),
            encoded(&line("dead", 3, old, interrupted)),
            // Recent, unclosed, attempts ended: finished but within the age.
            encoded(&line("dead", 4, recent, opened(work(2), None))),
            encoded(&line("dead", 5, recent, started(work(2), "dead", 2))),
            encoded(&line("dead", 6, recent, ended(work(2), "dead", 2))),
        ],
        "",
    );
    let journal = open(state.path(), "me");
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(report.dropped_works, 1);
    assert_eq!(report.dropped_lines, 3);
    let works: Vec<WorkId> = read_journal(state.path())
        .unwrap()
        .lines
        .iter()
        .map(|l| l.event.work().clone())
        .collect();
    assert_eq!(works, vec![work(2), work(2), work(2)]);
}

#[test]
fn compaction_never_drops_a_work_with_a_line_in_a_live_segment() {
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    plant(
        state.path(),
        "run-dead.jsonl",
        &[encoded(&line("dead", 1, old, opened(work(1), None)))],
        "",
    );
    let journal = open(state.path(), "me");
    // This (live) run writes a line of the same work.
    journal.record(Event::Linked {
        work: work(1),
        refs: Box::new(Refs::default()),
    });
    journal.flush();
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(report.consumed_files, 1);
    assert_eq!(report.dropped_works, 0);
}

#[test]
fn retention_keeps_the_most_recent_finished_works_and_their_ancestors() {
    let state = state();
    let recent = NOW_MS - DAY_MS;
    let mut lines = Vec::new();
    let mut seq = 0;
    let mut push = |ts: u64, event: Event, lines: &mut Vec<String>| {
        seq += 1;
        lines.push(encoded(&line("dead", seq, ts, event)));
    };
    // Work 1 is the oldest; it is the parent of work 5, the newest.
    for n in 1..=5u64 {
        let parent = (n == 5).then(|| work(1));
        push(recent + n, opened(work(n), parent), &mut lines);
        push(recent + n, closed(work(n)), &mut lines);
    }
    // An unfinished work (its attempt never ended) is never counted out.
    push(recent, opened(work(6), None), &mut lines);
    push(recent, started(work(6), "dead", 6), &mut lines);
    plant(state.path(), "run-dead.jsonl", &lines, "");

    let journal = open(state.path(), "me");
    let retention = Retention {
        max_age: RETENTION_AGE,
        max_finished_works: 2,
    };
    let report = journal.compact_with(NOW_MS, retention).unwrap();
    let read = read_journal(state.path()).unwrap();
    let mut kept: Vec<WorkId> = read.lines.iter().map(|l| l.event.work().clone()).collect();
    kept.sort();
    kept.dedup();
    // 5 and 4 are the two newest; 1 is 5's parent; 6 is unfinished.
    assert_eq!(kept, vec![work(1), work(4), work(5), work(6)]);
    assert_eq!(report.dropped_works, 2);
}

#[test]
fn compaction_folds_a_dead_compacted_file_and_removes_dead_locks() {
    let state = state();
    let first = open(state.path(), "first");
    first.record(opened(work(1), None));
    drop(first);
    // "second" folds "first"'s segment, then dies.
    let second = open(state.path(), "second");
    assert_eq!(
        second
            .compact_with(now_ms(), Retention::default())
            .unwrap()
            .consumed_files,
        1
    );
    let dir = journal_dir(state.path());
    assert!(!dir.join("run-first.jsonl").exists());
    assert!(!dir.join("run-first.lock").exists());
    second.record(closed(work(2)));
    drop(second);

    let third = open(state.path(), "third");
    // Real timestamps: compact as of now, so nothing is old enough to drop.
    let report = third.compact_with(now_ms(), Retention::default()).unwrap();
    // second's compacted file and its segment.
    assert_eq!(report.consumed_files, 2);
    assert!(!dir.join("compacted-second.jsonl").exists());
    assert!(!dir.join("run-second.jsonl").exists());
    let works: Vec<WorkId> = read_journal(state.path())
        .unwrap()
        .lines
        .iter()
        .map(|l| l.event.work().clone())
        .collect();
    assert_eq!(works, vec![work(1), work(2)]);
}

#[test]
fn compaction_is_skipped_while_another_daemon_holds_the_directory() {
    let state = state();
    let journal = open(state.path(), "me");
    plant(
        state.path(),
        "run-dead.jsonl",
        &[encoded(&line("dead", 1, 1, opened(work(1), None)))],
        "",
    );
    let dir = TrustedDir::anchor(&journal_dir(state.path())).unwrap();
    let held = maintenance_exclusive(&dir).unwrap().unwrap();
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(report.skipped_busy);
    assert!(journal_dir(state.path()).join("run-dead.jsonl").exists());
    drop(held);
    assert_eq!(
        journal
            .compact_with(NOW_MS, Retention::default())
            .unwrap()
            .consumed_files,
        1
    );
}

// ---------------------------------------------------------------------------
// The writer queue (finding B)
// ---------------------------------------------------------------------------

fn options_with(f: impl FnOnce(&mut JournalOptions)) -> JournalOptions {
    let mut options = options();
    f(&mut options);
    options
}

/// Wait (not time) for `done`, failing only if it never comes.
fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "never: {what}");
        std::thread::yield_now();
    }
}

#[test]
fn record_never_blocks_on_a_stalled_writer_and_drops_when_the_queue_is_full() {
    let state = state();
    let gate = WriterGate::closed();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "r1",
        &deck(),
        options_with(|o| {
            o.queue_capacity = 2;
            o.hooks.gate = Some(Arc::clone(&gate));
        }),
    )
    .unwrap();
    // The writer takes the first event and stalls on it.
    journal.record(opened(work(1), None));
    until("the writer holds the first event", || {
        gate.reached.load(Ordering::SeqCst) == 1
    });
    // Two fit the queue; every other one is dropped, and none of these calls
    // waits for the stalled writer.
    for n in 2..=50 {
        journal.record(closed(work(n)));
    }
    assert_eq!(journal.dropped_events(), 47);
    gate.open();
    journal.flush();
    let written = segment_lines(state.path(), "r1");
    // `seq` is given at write time: what was written is numbered without gaps,
    // in the order it was handed over.
    assert_eq!(written.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(
        written
            .iter()
            .map(|l| l.event.work().clone())
            .collect::<Vec<_>>(),
        [work(1), work(2), work(3)]
    );
}

#[test]
fn dropping_the_last_handle_writes_what_was_queued_and_frees_the_run() {
    let state = state();
    let first = open(state.path(), "first");
    for n in 1..=20 {
        first.record(closed(work(n)));
    }
    drop(first);
    assert_eq!(segment_lines(state.path(), "first").len(), 20);
    // Its lock is released: another run sees it dead and folds it.
    let second = open(state.path(), "second");
    let report = second.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(report.consumed_files, 1);
}

// ---------------------------------------------------------------------------
// Segment rotation (finding E)
// ---------------------------------------------------------------------------

#[test]
fn a_long_lived_runs_sealed_segments_are_compacted_without_a_restart() {
    let state = state();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| o.segment_max_bytes = 1024),
    )
    .unwrap();
    // A finished work, then enough lines of another to seal its segment.
    journal.try_record(opened(work(1), None)).unwrap();
    journal.try_record(started(work(1), "me", 1)).unwrap();
    journal.try_record(ended(work(1), "me", 1)).unwrap();
    journal.try_record(closed(work(1))).unwrap();
    for _ in 0..12 {
        journal
            .try_record(Event::Linked {
                work: work(2),
                refs: Box::new(Refs::default()),
            })
            .unwrap();
    }
    let dir = journal_dir(state.path());
    let segments = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("run-me") && n.ends_with(".jsonl"))
            .collect();
        names.sort();
        names
    };
    let before = segments(&dir);
    assert!(before.contains(&"run-me.jsonl".to_string()), "{before:?}");
    assert!(before.contains(&"run-me.1.jsonl".to_string()), "{before:?}");
    for name in &before {
        assert!(
            fs::metadata(dir.join(name)).unwrap().len() <= 1024,
            "{name}"
        );
    }
    let top = before
        .iter()
        .filter_map(|n| parse_segment(n).map(|(_, i)| i))
        .max()
        .unwrap();

    // Forty days on, with the daemon still up: the sealed segments fold, the
    // finished work goes, and the work with a line in the writable segment
    // stays.
    let later = now_ms() + 40 * DAY_MS;
    let report = journal.compact_with(later, Retention::default()).unwrap();
    assert_eq!(report.consumed_files as u64, top);
    assert_eq!(report.dropped_works, 1);
    assert_eq!(segments(&dir), vec![segment_name_n("me", top)]);
    let works: HashSet<WorkId> = read_journal(state.path())
        .unwrap()
        .lines
        .iter()
        .map(|l| l.event.work().clone())
        .collect();
    assert_eq!(works, HashSet::from([work(2)]));
    // The run goes on writing into its current segment.
    assert!(journal.try_record(closed(work(2))).unwrap() > 16);
}

#[test]
fn segment_names_parse_their_run_and_number() {
    assert_eq!(parse_segment("run-abc.jsonl"), Some(("abc", 0)));
    assert_eq!(parse_segment("run-abc.12.jsonl"), Some(("abc", 12)));
    assert_eq!(parse_segment("run-abc.012.jsonl"), None);
    assert_eq!(parse_segment("run-abc..jsonl"), None);
    assert_eq!(parse_segment("run-abc.x.jsonl"), None);
    assert_eq!(segment_name_n("abc", 0), "run-abc.jsonl");
    assert_eq!(segment_name_n("abc", 3), "run-abc.3.jsonl");
}

// ---------------------------------------------------------------------------
// Maintenance serialisation (finding C)
// ---------------------------------------------------------------------------

#[test]
fn recovery_held_off_past_the_old_fallback_waits_for_the_lock() {
    let state = state();
    plant(
        state.path(),
        "run-dead.jsonl",
        &[
            encoded(&line("dead", 1, 1, opened(work(1), None))),
            encoded(&line("dead", 2, 2, started(work(1), "dead", 1))),
        ],
        "",
    );
    let journal = WorkJournal::try_open_with(
        state.path(),
        "new",
        &deck(),
        options_with(|o| o.recovery_retry = Duration::from_millis(1)),
    )
    .unwrap();
    let dir = TrustedDir::anchor(&journal_dir(state.path())).unwrap();
    // Another daemon's maintenance holds the directory.
    let held = maintenance_exclusive(&dir).unwrap().unwrap();
    assert_eq!(journal.start_recovery(), RecoveryStart::Deferred);
    // Well past the 50 tries (5 seconds) after which recovery used to go ahead
    // without the lock.
    until("recovery tried the lock 60 times", || {
        journal.recovery_attempts() >= 60
    });
    assert!(!journal.recovery_done());
    journal.flush();
    assert!(
        segment_lines(state.path(), "new").is_empty(),
        "recovered unlocked"
    );
    // Compaction waits for recovery.
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(report.skipped_recovery_pending);
    assert!(journal_dir(state.path()).join("run-dead.jsonl").exists());

    drop(held);
    assert!(journal.wait_recovered(Duration::from_secs(60)));
    assert_eq!(
        interrupted_attempts(&segment_lines(state.path(), "new")),
        vec![("dead/1".to_string(), work(1))]
    );
    assert_eq!(
        journal
            .compact_with(NOW_MS, Retention::default())
            .unwrap()
            .consumed_files,
        1
    );
}

#[test]
fn a_free_lock_recovers_at_startup_at_once() {
    let state = state();
    plant(
        state.path(),
        "run-dead.jsonl",
        &[encoded(&line("dead", 1, 1, started(work(1), "dead", 1)))],
        "",
    );
    let journal = open(state.path(), "new");
    match journal.start_recovery() {
        RecoveryStart::Done(report) => assert_eq!(report.interrupted, 1),
        other => panic!("recovery was not run at once: {other:?}"),
    }
    assert!(journal.recovery_done());
}

#[test]
fn a_reader_racing_a_compactors_publish_and_unlink_keeps_every_retained_work() {
    let state = state();
    // Two dead runs' segments, each with an open (so retained) work.
    plant(
        state.path(),
        "run-deada.jsonl",
        &[
            encoded(&line("deada", 1, 1, opened(work(1), None))),
            encoded(&line("deada", 2, 2, started(work(1), "deada", 1))),
        ],
        "",
    );
    plant(
        state.path(),
        "run-deadb.jsonl",
        &[
            encoded(&line("deadb", 1, 3, opened(work(2), None))),
            encoded(&line("deadb", 2, 4, started(work(2), "deadb", 1))),
        ],
        "",
    );
    let reader: Arc<Mutex<Option<std::thread::JoinHandle<JournalRead>>>> =
        Arc::new(Mutex::new(None));
    let state_path = state.path().to_path_buf();
    let slot = Arc::clone(&reader);
    // Between the compactor publishing its output and unlinking the files it
    // folded, a reader starts. The compactor goes on only once that reader
    // has found the maintenance lock held and is about to wait for it, so
    // the reader provably reads after the unlinks, not before the publish.
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let (waiting, wait) = mpsc::channel();
        let path = state_path.clone();
        let handle = std::thread::spawn(move || {
            ON_SHARED_WAIT.with(|seam| *seam.borrow_mut() = Some(waiting));
            read_journal(&path).unwrap()
        });
        wait.recv_timeout(Duration::from_secs(60))
            .expect("the reader never waited on the compactor's lock");
        *slot.lock().unwrap() = Some(handle);
    });
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| o.hooks.after_publish = Some(hook)),
    )
    .unwrap();
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(report.consumed_files, 2);
    assert_eq!(report.written_files, 1);
    let handle = reader.lock().unwrap().take().expect("the hook ran");
    let read = handle.join().unwrap();
    let works: HashSet<WorkId> = read.lines.iter().map(|l| l.event.work().clone()).collect();
    assert_eq!(works, HashSet::from([work(1), work(2)]));
    assert_eq!(read.stats.duplicates, 0, "it read a half-done compaction");
}

#[test]
fn a_reader_holding_the_directory_keeps_compaction_out() {
    let state = state();
    plant(
        state.path(),
        "run-dead.jsonl",
        &[encoded(&line("dead", 1, 1, opened(work(1), None)))],
        "",
    );
    let journal = open(state.path(), "me");
    let dir = TrustedDir::anchor(&journal_dir(state.path())).unwrap();
    let reading = maintenance_shared(&dir).unwrap();
    assert!(
        journal
            .compact_with(NOW_MS, Retention::default())
            .unwrap()
            .skipped_busy
    );
    assert!(journal.try_recover().unwrap().busy);
    drop(reading);
    assert_eq!(
        journal
            .compact_with(NOW_MS, Retention::default())
            .unwrap()
            .consumed_files,
        1
    );
}

// ---------------------------------------------------------------------------
// Budgets (finding D)
// ---------------------------------------------------------------------------

fn small(files: usize, bytes: u64, entries: usize) -> Budget {
    Budget {
        files,
        bytes,
        entries,
    }
}

#[test]
fn many_short_junk_lines_are_read_within_budget_and_not_all_carried() {
    let state = state();
    let mut text = String::new();
    for n in 0..100_000 {
        text.push_str(&format!("j{n}\n"));
    }
    text.push_str(&encoded(&line("dead", 1, 1, started(work(1), "dead", 1))));
    text.push('\n');
    fs::write(make_dir(state.path()).join("run-dead.jsonl"), text).unwrap();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| {
            o.budgets.carried_unreadable = 100;
            o.budgets.recovery = small(16, 64 * 1024, 1000);
        }),
    )
    .unwrap();
    // Over recovery's byte budget: it writes nothing rather than guess.
    let report = journal.try_recover().unwrap();
    assert!(report.incomplete);
    assert_eq!(report.interrupted, 0);
    journal.flush();
    assert!(segment_lines(state.path(), "me").is_empty());
    // Compaction carries no more junk than its budget allows.
    let compacted = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(compacted.carried_unreadable, 100);
    assert_eq!(compacted.discarded, 100_000 - 100);
    let read = read_journal(state.path()).unwrap();
    assert_eq!(read.stats.unparsable, 100);
    assert_eq!(read.lines.len(), 1);
}

#[test]
fn a_round_drops_only_the_works_whose_files_it_rewrites_within_its_budget() {
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    for n in 1..=12u64 {
        plant(
            state.path(),
            &format!("run-dead{n:02}.jsonl"),
            &[
                encoded(&line(&format!("dead{n:02}"), 1, old, opened(work(n), None))),
                encoded(&line(&format!("dead{n:02}"), 2, old, closed(work(n)))),
            ],
            "",
        );
    }
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        // Five files a round.
        options_with(|o| o.budgets.compaction = small(5, 1 << 30, 1 << 20)),
    )
    .unwrap();
    let dead_left = || {
        fs::read_dir(journal_dir(state.path()))
            .unwrap()
            .filter(|e| {
                let name = e.as_ref().unwrap().file_name();
                name.to_str().unwrap().starts_with("run-dead")
            })
            .count()
    };
    let first = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(first.over_budget);
    assert!(!first.retention_skipped);
    assert_eq!((first.consumed_files, first.dropped_works), (5, 5));
    assert_eq!(first.written_files, 0, "everything it read was dropped");
    assert_eq!(dead_left(), 7);
    let second = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!((second.consumed_files, second.dropped_works), (5, 5));
    let third = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!((third.consumed_files, third.dropped_works), (2, 2));
    assert!(!third.over_budget);
    assert_eq!(dead_left(), 0);
    assert!(read_journal(state.path()).unwrap().lines.is_empty());
}

/// Reviewer finding 1 / audit N3: finished history far over one round's byte
/// budget, spread over many files, converges to the retention limit over
/// rounds, with no file that every round has to read whole again; and once it
/// has, a new dead segment and this run's own sealed segments are still
/// folded.
#[test]
fn over_budget_finished_history_converges_to_the_retention_limit() {
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    let recent = NOW_MS - DAY_MS;
    // Works 1..=50 old, 51..=60 recent, all finished, two to a file; work 61
    // is unfinished and a child of work 1.
    for n in 1..=30u64 {
        let run = format!("dead{n:02}");
        let mut lines = Vec::new();
        for (i, w) in [2 * n - 1, 2 * n].into_iter().enumerate() {
            let ts = if w <= 50 { old + w } else { recent + w };
            let seq = 4 * i as u64;
            lines.push(encoded(&line(&run, seq + 1, ts, opened(work(w), None))));
            lines.push(encoded(&line(&run, seq + 2, ts, started(work(w), &run, w))));
            lines.push(encoded(&line(&run, seq + 3, ts, ended(work(w), &run, w))));
            lines.push(encoded(&line(&run, seq + 4, ts, closed(work(w)))));
        }
        if n == 1 {
            lines.push(encoded(&line(
                &run,
                20,
                old,
                opened(work(61), Some(work(1))),
            )));
            lines.push(encoded(&line(&run, 21, old, started(work(61), &run, 61))));
        }
        plant(state.path(), &format!("run-{run}.jsonl"), &lines, "");
    }
    let total: u64 = fs::read_dir(journal_dir(state.path()))
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    let budget = 6 * 1024;
    assert!(
        total > 4 * budget,
        "the history is not over budget: {total}"
    );
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| {
            o.budgets.compaction = small(4, budget, 10_000);
            o.compacted_chunk_bytes = 3 * 1024;
            o.segment_max_bytes = 1024;
        }),
    )
    .unwrap();
    let retention = Retention {
        max_age: RETENTION_AGE,
        max_finished_works: 4,
    };
    let converge = |journal: &WorkJournal| {
        for round in 0..200 {
            let report = journal.compact_with(NOW_MS, retention).unwrap();
            assert!(!report.retention_skipped, "round {round}: {report:?}");
            if report.consumed_files == 0 {
                return round;
            }
        }
        panic!("compaction never reached a fixed point");
    };
    let rounds = converge(&journal);
    assert!(rounds > 1, "it all fit in one round");
    let kept = |state: &Path| -> BTreeSet<WorkId> {
        read_journal(state)
            .unwrap()
            .lines
            .iter()
            .map(|l| l.event.work().clone())
            .collect()
    };
    // The four newest finished works, the unfinished one and its parent.
    let mut expected: BTreeSet<WorkId> = [1, 57, 58, 59, 60, 61].into_iter().map(work).collect();
    assert_eq!(kept(state.path()), expected);
    let names = || -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(journal_dir(state.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".jsonl"))
            .collect();
        names.sort();
        names
    };
    assert!(
        !names().iter().any(|n| n.starts_with("run-dead")),
        "{:?}",
        names()
    );
    // Every chunk fits the round's budget, so none is too big to fold again.
    for name in names() {
        let len = fs::metadata(journal_dir(state.path()).join(&name))
            .unwrap()
            .len();
        assert!(len <= budget, "{name} is {len} bytes");
    }

    // Afterwards: a newer finished work in a new dead segment, and this run's
    // own sealed segments.
    plant(
        state.path(),
        "run-late.jsonl",
        &[
            encoded(&line("late", 1, recent + 100, opened(work(62), None))),
            encoded(&line("late", 2, recent + 100, closed(work(62)))),
        ],
        "",
    );
    for _ in 0..12 {
        journal
            .try_record(Event::Linked {
                work: work(61),
                refs: Box::new(Refs::default()),
            })
            .unwrap();
    }
    let sealed = || names().iter().filter(|n| n.starts_with("run-me")).count();
    assert!(
        sealed() > 1,
        "nothing of this run was sealed: {:?}",
        names()
    );
    converge(&journal);
    assert!(!names().contains(&"run-late.jsonl".to_string()));
    assert_eq!(sealed(), 1, "{:?}", names());
    expected.remove(&work(57));
    expected.insert(work(62));
    assert_eq!(kept(state.path()), expected);
}

/// Reviewer finding 2 / audit N2: a round holds at most its budgets'
/// de-duplication entries while it reads, however many short lines a file
/// has: unreadable lines past the carry budget are discarded as they are
/// read, never collected.
#[test]
fn compaction_holds_no_more_than_its_budgets_while_it_reads() {
    let state = state();
    let recent = NOW_MS - DAY_MS;
    let mut text = String::new();
    for n in 1..=10u64 {
        text.push_str(&encoded(&line(
            "dead",
            2 * n - 1,
            recent,
            opened(work(n), None),
        )));
        text.push('\n');
        text.push_str(&encoded(&line("dead", 2 * n, recent, closed(work(n)))));
        text.push('\n');
    }
    for _ in 0..100_000 {
        text.push_str("x\n");
    }
    for n in 0..100_000 {
        text.push_str(&format!("j{n}\n"));
    }
    fs::write(make_dir(state.path()).join("run-dead.jsonl"), text).unwrap();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| {
            o.budgets.carried_unreadable = 5;
            o.budgets.compaction = small(16, 64 * MIB, 40);
        }),
    )
    .unwrap();
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(!report.retention_skipped, "{report:?}");
    assert_eq!(report.consumed_files, 1);
    assert_eq!(report.kept_lines, 20);
    assert_eq!(report.carried_unreadable, 5);
    // `x` once, `j0`..`j3`; every other `j` discarded, the repeated `x`
    // de-duplicated.
    assert_eq!(report.discarded, 100_000 - 4);
    assert!(
        report.held_peak <= 5 + 40,
        "held {} entries at once",
        report.held_peak
    );
    assert!(
        report.held_peak_bytes < 2048,
        "held {} bytes at once",
        report.held_peak_bytes
    );
    let read = read_journal(state.path()).unwrap();
    assert_eq!(read.lines.len(), 20);
    assert_eq!(read.stats.unparsable, 5);
}

/// A kept work's attempt may be ended by another work's line (a delegation's
/// attempt is the worker generation, whose end is written on the
/// orchestration's work). Dropping that other work would leave the kept one
/// unfinished for good, so it is kept with it.
#[test]
fn a_work_holding_a_kept_works_attempt_end_is_kept() {
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    let recent = NOW_MS - DAY_MS;
    plant(
        state.path(),
        "run-dead.jsonl",
        &[
            encoded(&line("dead", 1, old, opened(work(1), None))),
            encoded(&line("dead", 2, old, started(work(1), "dead", 7))),
            // The delegation: no parent, recent, its attempt is agent 7's.
            encoded(&line("dead", 3, recent, opened(work(2), None))),
            encoded(&line("dead", 4, recent, started(work(2), "dead", 7))),
            // Agent 7's end, written on work 1.
            encoded(&line("dead", 5, old, ended(work(1), "dead", 7))),
        ],
        "",
    );
    let journal = open(state.path(), "me");
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(report.dropped_works, 0, "{report:?}");
    assert_eq!(report.kept_lines, 5);
}

/// Audit N4: a listing that stops at its cap between a live run's segment and
/// that run's lock. The lock is probed by name anyway, so the live run's
/// writable segment stays where it is and later appends remain readable; and
/// the truncated listing drops no work and removes no lock.
#[test]
fn a_truncated_listing_never_folds_a_live_runs_writable_segment() {
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    let other = open(state.path(), "other");
    other.try_record(opened(work(1), None)).unwrap();
    plant(
        state.path(),
        "run-dead.jsonl",
        &[
            encoded(&line("dead", 1, old, opened(work(2), None))),
            encoded(&line("dead", 2, old, closed(work(2)))),
        ],
        "",
    );
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| {
            o.hooks.listing_omits = Some(Arc::new(|name: &str| name == "run-other.lock"));
        }),
    )
    .unwrap();
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(report.retention_skipped, "{report:?}");
    assert_eq!(report.dropped_works, 0);
    assert_eq!(report.consumed_files, 1, "only the dead segment folds");
    let dir = journal_dir(state.path());
    assert!(dir.join("run-other.jsonl").exists());
    assert!(dir.join("run-other.lock").exists());
    other.try_record(closed(work(1))).unwrap();
    let read = read_journal(state.path()).unwrap();
    let of_one: Vec<&str> = read
        .lines
        .iter()
        .filter(|l| l.event.work() == &work(1))
        .map(|l| l.event.kind())
        .collect();
    assert_eq!(of_one, ["opened", "closed"]);
    assert_eq!(
        read.lines
            .iter()
            .filter(|l| l.event.work() == &work(2))
            .count(),
        2,
        "a truncated listing dropped a work"
    );
}

/// Reviewer 3 / audit S1 and D1: more distinct finished works than the
/// compaction index's entry budget. Every round reports the saturation and
/// drops nothing (no work and no dead run's lock); folding the files into one
/// chunk does not cure it, since the distinct works are still there. Only a
/// budget that fits lets retention run again.
#[test]
fn an_index_saturated_round_drops_nothing_and_reports_it() {
    let state = state();
    let old = NOW_MS - 40 * DAY_MS;
    for n in 1..=3u64 {
        let run = format!("dead{n}");
        let lines: Vec<String> = [2 * n - 1, 2 * n]
            .into_iter()
            .enumerate()
            .flat_map(|(i, w)| {
                let seq = 4 * i as u64;
                [
                    encoded(&line(&run, seq + 1, old + w, opened(work(w), None))),
                    encoded(&line(&run, seq + 2, old + w, started(work(w), &run, w))),
                    encoded(&line(&run, seq + 3, old + w, ended(work(w), &run, w))),
                    encoded(&line(&run, seq + 4, old + w, closed(work(w)))),
                ]
            })
            .collect();
        plant(state.path(), &format!("run-{run}.jsonl"), &lines, "");
    }
    let dir = journal_dir(state.path());
    fs::write(dir.join("run-gone.lock"), "").unwrap();
    let all_lines = read_journal(state.path()).unwrap().lines.len();
    let saturated = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| o.budgets.compaction = small(1024, 1 << 30, 4)),
    )
    .unwrap();
    for round in 0..3 {
        let report = saturated
            .compact_with(NOW_MS, Retention::default())
            .unwrap();
        assert!(report.index_saturated, "round {round}: {report:?}");
        assert!(report.retention_skipped, "round {round}: {report:?}");
        assert_eq!(report.dropped_works, 0, "round {round}: {report:?}");
        assert_eq!(report.dropped_lines, 0, "round {round}: {report:?}");
        assert!(
            dir.join("run-gone.lock").exists(),
            "round {round}: an index-full round removed a dead run's lock"
        );
        assert_eq!(read_journal(state.path()).unwrap().lines.len(), all_lines);
    }
    let left: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("run-dead"))
        .collect();
    assert!(left.is_empty(), "the files were not folded: {left:?}");
    drop(saturated);

    // A budget that fits the index: retention runs and the dead lock goes.
    let fits = open(state.path(), "me2");
    let report = fits.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(!report.index_saturated, "{report:?}");
    assert!(!report.retention_skipped, "{report:?}");
    assert_eq!(report.dropped_works, 6, "{report:?}");
    assert!(!dir.join("run-gone.lock").exists());
}

/// Reviewer 2 / audit S2: a probe for a free chunk name that fails with
/// anything but NotFound fails the round instead of probing names for ever
/// under the maintenance lock. The round abandons what it wrote, its inputs
/// stay, and the lock is free for the next reader.
#[test]
fn a_chunk_name_probe_error_fails_the_round_and_keeps_its_inputs() {
    let state = state();
    // Recent, so the round keeps its lines and has a chunk to publish.
    let old = NOW_MS - DAY_MS;
    plant(
        state.path(),
        "run-dead.jsonl",
        &[
            encoded(&line("dead", 1, old, opened(work(1), None))),
            encoded(&line("dead", 2, old, closed(work(1)))),
        ],
        "",
    );
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| o.hooks.chunk_probe_fault = Some(io::ErrorKind::PermissionDenied)),
    )
    .unwrap();
    let error = journal
        .compact_with(NOW_MS, Retention::default())
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    let dir = journal_dir(state.path());
    assert!(dir.join("run-dead.jsonl").exists());
    let stray: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("compacted"))
        .collect();
    assert!(stray.is_empty(), "the failed round left output: {stray:?}");
    let read = journal.read().unwrap();
    assert_eq!(
        read.lines
            .iter()
            .filter(|l| l.event.work() == &work(1))
            .count(),
        2
    );
}

#[test]
fn one_huge_line_is_skipped_without_being_held() {
    let state = state();
    let mut bytes = vec![b'x'; 8 * 1024 * 1024];
    bytes.push(b'\n');
    bytes.extend_from_slice(encoded(&line("dead", 1, 1, started(work(1), "dead", 1))).as_bytes());
    bytes.push(b'\n');
    fs::write(make_dir(state.path()).join("run-dead.jsonl"), bytes).unwrap();
    let read = read_journal(state.path()).unwrap();
    assert_eq!(read.stats.too_long, 1);
    assert_eq!(read.lines.len(), 1);
    let journal = open(state.path(), "me");
    assert_eq!(journal.try_recover().unwrap().interrupted, 1);
    let compacted = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert_eq!(compacted.discarded, 1);
}

#[test]
fn recovery_over_its_entry_budget_writes_nothing() {
    let state = state();
    plant(
        state.path(),
        "run-dead.jsonl",
        &[
            encoded(&line("dead", 1, 1, started(work(1), "dead", 1))),
            encoded(&line("dead", 2, 2, started(work(2), "dead", 2))),
        ],
        "",
    );
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| o.budgets.recovery = small(16, 1 << 30, 1)),
    )
    .unwrap();
    let report = journal.try_recover().unwrap();
    assert!(report.incomplete);
    assert_eq!(report.interrupted, 0);
}

/// Reviewer finding 4 / audit N5: a startup recovery over its file budget
/// does not count as done. Compaction still runs, folds the dead segments
/// into one chunk, and recovery, tried again after that round, completes in
/// the same run.
#[test]
fn an_over_budget_startup_recovery_completes_once_compaction_reduces_the_input() {
    let state = state();
    for n in 1..=6u64 {
        let run = format!("dead{n}");
        plant(
            state.path(),
            &format!("run-{run}.jsonl"),
            &[
                encoded(&line(&run, 1, n, opened(work(n), None))),
                encoded(&line(&run, 2, n, started(work(n), &run, 1))),
            ],
            "",
        );
    }
    let journal = WorkJournal::try_open_with(
        state.path(),
        "new",
        &deck(),
        options_with(|o| {
            o.budgets.recovery = small(4, 1 << 30, 1 << 20);
            // Only a compaction can bring it about within the test.
            o.recovery_retry = Duration::from_secs(3600);
            o.recovery_backoff_max = Duration::from_secs(3600);
        }),
    )
    .unwrap();
    match journal.start_recovery() {
        RecoveryStart::Retrying(report) => {
            assert!(report.incomplete, "{report:?}");
            assert_eq!(report.interrupted, 0);
        }
        other => panic!("an over-budget recovery read as {other:?}"),
    }
    assert!(!journal.recovery_done());
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(!report.skipped_recovery_pending, "compaction waited");
    assert_eq!(report.consumed_files, 6);
    assert_eq!(report.written_files, 1);
    assert!(report.recovery_retried);
    assert!(journal.recovery_done());
    journal.flush();
    let mut interrupted = interrupted_attempts(&segment_lines(state.path(), "new"));
    interrupted.sort();
    let expected: Vec<(String, WorkId)> = (1..=6u64)
        .map(|n| (format!("dead{n}/1"), work(n)))
        .collect();
    assert_eq!(interrupted, expected);
}

/// A startup recovery that fails with an I/O error keeps the error in its
/// report and is retried in the background until it completes.
#[test]
fn a_failed_startup_recovery_is_retried_until_it_completes() {
    let state = state();
    plant(
        state.path(),
        "run-dead.jsonl",
        &[encoded(&line("dead", 1, 1, started(work(1), "dead", 1)))],
        "",
    );
    let failures = Arc::new(AtomicU64::new(2));
    let journal = WorkJournal::try_open_with(
        state.path(),
        "new",
        &deck(),
        options_with(|o| {
            o.hooks.fail_recovery = Some(Arc::clone(&failures));
            o.recovery_retry = Duration::from_millis(1);
            o.recovery_backoff_max = Duration::from_millis(4);
        }),
    )
    .unwrap();
    match journal.start_recovery() {
        RecoveryStart::Retrying(report) => {
            assert!(report.error.is_some(), "{report:?}");
            assert!(!report.completed());
        }
        other => panic!("a failed recovery read as {other:?}"),
    }
    assert!(journal.wait_recovered(Duration::from_secs(60)));
    assert_eq!(failures.load(Ordering::SeqCst), 0);
    assert!(journal.recovery_attempts() >= 3);
    journal.flush();
    assert_eq!(
        interrupted_attempts(&segment_lines(state.path(), "new")),
        vec![("dead/1".to_string(), work(1))]
    );
}

#[test]
fn a_read_over_its_budget_says_it_is_partial() {
    let state = state();
    for n in 1..=3u64 {
        plant(
            state.path(),
            &format!("run-dead{n}.jsonl"),
            &[encoded(&line(
                &format!("dead{n}"),
                1,
                n,
                opened(work(n), None),
            ))],
            "",
        );
    }
    let read = read_journal_with(state.path(), small(2, 1 << 30, 1 << 20)).unwrap();
    assert!(read.stats.truncated);
    assert_eq!(read.lines.len(), 2);
    let read = read_journal_with(state.path(), small(16, 1 << 30, 1)).unwrap();
    assert!(read.stats.truncated);
    assert_eq!(read.lines.len(), 1);
    assert!(!read_journal(state.path()).unwrap().stats.truncated);
}

// ---------------------------------------------------------------------------
// Redaction by resolved path (finding F)
// ---------------------------------------------------------------------------

#[test]
fn a_workspace_with_dot_components_is_redacted_as_resolved() {
    let home = home();
    let ws = |p: &str| {
        Workspace::from_path_with_home(Path::new(p), &home)
            .unwrap()
            .as_str()
            .to_string()
    };
    assert_eq!(ws("/home/../home/someone/code/x"), "~/code/x");
    assert_eq!(ws("/home/someone/./code/../code/x"), "~/code/x");
    assert_eq!(ws("/opt/../home/someone"), "~");
    assert_eq!(ws("/../../home/someone/x"), "~/x");
    // A path that leaves home is written as resolved.
    assert_eq!(ws("/home/someone/../other/x"), "/home/other/x");
    // A home spelled with dot components redacts the same.
    assert_eq!(
        Workspace::from_path_with_home(
            Path::new("/home/someone/code/x"),
            Path::new("/home/./someone/")
        )
        .unwrap()
        .as_str(),
        "~/code/x"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_home_is_redacted_in_either_spelling() {
    let root = state();
    let real = root.path().join("real-home");
    fs::create_dir_all(real.join("code/x")).unwrap();
    let link = root.path().join("home-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // HOME is the link; the workspace arrives by the real path.
    assert_eq!(
        Workspace::from_path_with_home(&real.join("code/x"), &link)
            .unwrap()
            .as_str(),
        "~/code/x"
    );
    // HOME is the real path; the workspace arrives through the link.
    assert_eq!(
        Workspace::from_path_with_home(&link.join("code/x"), &real)
            .unwrap()
            .as_str(),
        "~/code/x"
    );
    // Through the handle too, and the line it writes holds neither spelling.
    let state = state();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "r1",
        &deck(),
        JournalOptions {
            home: link.clone(),
            ..JournalOptions::default()
        },
    )
    .unwrap();
    let workspace = journal.workspace(&real.join("code/x")).unwrap();
    assert_eq!(workspace.as_str(), "~/code/x");
    journal
        .try_record(Event::Opened {
            work: work(1),
            launch: Launch::Pane,
            parent: None,
            refs: Box::default(),
            workspace: Some(workspace),
        })
        .unwrap();
    let text = fs::read_to_string(journal_dir(state.path()).join(segment_name("r1"))).unwrap();
    assert!(!text.contains(real.to_str().unwrap()), "{text}");
    assert!(!text.contains(link.to_str().unwrap()), "{text}");
}

/// Qodo finding 5 on PR #2248: a rotation whose directory sync fails does
/// not switch to the new segment. The run keeps writing its current segment
/// (every line still reads back), no unsynced empty segment is left behind,
/// and once the sync succeeds the next rotation goes ahead.
#[test]
fn a_rotation_whose_directory_sync_fails_keeps_writing_the_current_segment() {
    let state = state();
    let faults = Arc::new(AtomicU64::new(3));
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| {
            o.segment_max_bytes = 512;
            o.hooks.rotate_sync_fault = Some(Arc::clone(&faults));
        }),
    )
    .unwrap();
    let dir = journal_dir(state.path());
    let segments = || -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with("run-me") && n.ends_with(".jsonl"))
            .collect();
        names.sort();
        names
    };
    let linked = |n: u64| Event::Linked {
        work: work(n),
        refs: Box::new(Refs::default()),
    };
    // Past the segment size: each rotation tried meets a failing sync, while
    // three failures last.
    let mut written = 0;
    while faults.load(Ordering::SeqCst) > 0 {
        journal.try_record(linked(written)).unwrap();
        written += 1;
        assert!(written < 100, "the rotation never reached its sync");
    }
    assert_eq!(segments(), vec!["run-me.jsonl".to_string()]);
    assert!(fs::metadata(dir.join("run-me.jsonl")).unwrap().len() > 512);
    // The sync works again: the next rotation tried goes ahead.
    while segments().len() == 1 {
        journal.try_record(linked(written)).unwrap();
        written += 1;
        assert!(written < 200, "rotation never resumed");
    }
    assert_eq!(
        segments(),
        vec!["run-me.1.jsonl".to_string(), "run-me.jsonl".to_string()]
    );
    let read: HashSet<WorkId> = read_journal(state.path())
        .unwrap()
        .lines
        .iter()
        .map(|l| l.event.work().clone())
        .collect();
    assert_eq!(read, (0..written).map(work).collect::<HashSet<_>>());
}

/// Qodo finding 10 (second round) on PR #2248: a rotation that keeps
/// failing is not tried again on every line, but once the segment has grown
/// further, with the stretch doubling per failure. And an empty segment a
/// failed rotation could not remove does not wedge every later rotation:
/// once the fault clears, the writer skips that name for the next free one.
#[test]
fn a_failing_rotation_backs_off_and_a_leftover_segment_does_not_wedge_it() {
    let state = state();
    let faults = Arc::new(AtomicU64::new(u64::MAX));
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| {
            o.segment_max_bytes = 4096;
            o.hooks.rotate_sync_fault = Some(Arc::clone(&faults));
        }),
    )
    .unwrap();
    let dir = journal_dir(state.path());
    let linked = |n: u64| Event::Linked {
        work: work(n),
        refs: Box::new(Refs::default()),
    };
    let current_len = || fs::metadata(dir.join("run-me.jsonl")).unwrap().len();
    // To four times the segment size, with every directory sync failing.
    let mut written = 0;
    let mut over = 0;
    while current_len() < 4096 * 4 {
        if current_len() >= 4096 {
            over += 1;
        }
        journal.try_record(linked(written)).unwrap();
        written += 1;
        assert!(written < 2000, "the segment never grew");
    }
    let attempts = u64::MAX - faults.load(Ordering::SeqCst);
    assert!(attempts >= 1, "rotation was never tried");
    // Retried after a sixteenth of the size more, doubling up to the size:
    // seven attempts over three segments' worth of lines, not one per line.
    assert!(
        attempts <= 8 && attempts * 2 < over,
        "{attempts} rotation attempts over {over} lines past the segment size"
    );
    // The unsynced segment was removed each time.
    assert!(!dir.join("run-me.1.jsonl").exists());

    // An earlier failed rotation could not remove its file: it is left at
    // the next segment's name.
    plant(state.path(), "run-me.1.jsonl", &[], "");
    faults.store(0, Ordering::SeqCst);
    let before = written;
    while !dir.join("run-me.2.jsonl").exists() {
        journal.try_record(linked(written)).unwrap();
        written += 1;
        assert!(written < before + 1000, "rotation stayed wedged");
    }
    journal.try_record(linked(written)).unwrap();
    written += 1;
    journal.flush();
    assert!(fs::metadata(dir.join("run-me.2.jsonl")).unwrap().len() > 0);
    let read: HashSet<WorkId> = read_journal(state.path())
        .unwrap()
        .lines
        .iter()
        .map(|l| l.event.work().clone())
        .collect();
    assert_eq!(read, (0..written).map(work).collect::<HashSet<_>>());
}

/// Greptile P1 on PR #2248: a live daemon's exited-but-open pane keeps its
/// work id and can be restarted into it, so retention keeps that work — its
/// `opened` included — even once rotation has sealed its lines and they are
/// past the age limit. A delegation of the live run, which gets no new
/// attempt, and a pane work the run closed still go; and once the run is
/// gone, its unclosed pane work expires too.
#[test]
fn retention_keeps_an_open_restartable_pane_work_of_a_live_run() {
    let state = state();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "me",
        &deck(),
        options_with(|o| o.segment_max_bytes = 1024),
    )
    .unwrap();
    // work(1): a pane whose agent exited, the pane still open.
    journal.try_record(opened(work(1), None)).unwrap();
    journal.try_record(started(work(1), "me", 1)).unwrap();
    journal.try_record(ended(work(1), "me", 1)).unwrap();
    // work(2): a pane the run closed.
    journal.try_record(opened(work(2), None)).unwrap();
    journal.try_record(started(work(2), "me", 2)).unwrap();
    journal.try_record(ended(work(2), "me", 2)).unwrap();
    journal.try_record(closed(work(2))).unwrap();
    // work(3): a delegation whose worker generation ended, never reported.
    journal
        .try_record(Event::Opened {
            work: work(3),
            launch: Launch::Delegation,
            parent: None,
            refs: Box::new(Refs::default()),
            workspace: None,
        })
        .unwrap();
    journal.try_record(started(work(3), "me", 3)).unwrap();
    journal.try_record(ended(work(3), "me", 3)).unwrap();
    // Enough of another work to seal those lines into older segments.
    for _ in 0..12 {
        journal
            .try_record(Event::Linked {
                work: work(9),
                refs: Box::new(Refs::default()),
            })
            .unwrap();
    }
    let works = |state: &Path| -> HashSet<WorkId> {
        read_journal(state)
            .unwrap()
            .lines
            .iter()
            .map(|l| l.event.work().clone())
            .collect()
    };
    let later = now_ms() + 40 * DAY_MS;
    let report = journal.compact_with(later, Retention::default()).unwrap();
    assert_eq!(report.dropped_works, 2, "{report:?}");
    assert_eq!(works(state.path()), HashSet::from([work(1), work(9)]));
    let kinds: Vec<&str> = read_journal(state.path())
        .unwrap()
        .lines
        .iter()
        .filter(|l| l.event.work() == &work(1))
        .map(|l| l.event.kind())
        .collect();
    assert_eq!(kinds, ["opened", "attempt_started", "attempt_ended"]);

    // The run goes away: its unclosed pane work is finished, and expires.
    drop(journal);
    let next = open(state.path(), "next");
    next.compact_with(later, Retention::default()).unwrap();
    assert!(!works(state.path()).contains(&work(1)));
}

/// Qodo finding 7 on PR #2248: the daemon's startup recovery runs entirely
/// off the caller's thread. With every recovery attempt held at a gate, the
/// start returns at once (it would never return if it ran the first attempt
/// itself), compaction waits for the recovery it owes, and once the gate
/// opens recovery completes and compaction runs.
#[test]
fn startup_recovery_in_the_background_never_holds_up_its_caller() {
    let state = state();
    plant(
        state.path(),
        "run-dead.jsonl",
        &[encoded(&line("dead", 1, 1, started(work(1), "dead", 1)))],
        "",
    );
    let gate = WriterGate::closed();
    let journal = WorkJournal::try_open_with(
        state.path(),
        "new",
        &deck(),
        options_with(|o| o.hooks.recovery_gate = Some(Arc::clone(&gate))),
    )
    .unwrap();
    journal.start_recovery_in_background();
    // Returned with the first attempt still held.
    assert!(!journal.recovery_done());
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(report.skipped_recovery_pending, "{report:?}");
    until("the background attempt reaches its gate", || {
        gate.reached.load(Ordering::SeqCst) > 0
    });
    gate.open();
    assert!(journal.wait_recovered(Duration::from_secs(60)));
    assert!(journal.recovery_done());
    assert_eq!(
        interrupted_attempts(&segment_lines(state.path(), "new")).len(),
        1
    );
    let report = journal.compact_with(NOW_MS, Retention::default()).unwrap();
    assert!(!report.skipped_recovery_pending, "{report:?}");
}

/// Qodo finding 2 (second round) on PR #2248: the daemon's compaction loop
/// starts while startup recovery's first attempt is still running on its
/// thread, so its immediate round is skipped. That round is not lost to the
/// next hourly tick: it runs as soon as the first attempt has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_round_held_back_by_startup_recovery_runs_once_it_has() {
    let state = state();
    let dead = plant(
        state.path(),
        "run-dead.jsonl",
        &[encoded(&line("dead", 1, 1, started(work(1), "dead", 1)))],
        "",
    );
    let gate = WriterGate::closed();
    let skipped = Arc::new(AtomicU64::new(0));
    let journal = WorkJournal::try_open_with(
        state.path(),
        "new",
        &deck(),
        options_with(|o| {
            o.hooks.recovery_gate = Some(Arc::clone(&gate));
            o.hooks.skipped_pending = Some(Arc::clone(&skipped));
        }),
    )
    .unwrap();
    journal.start_recovery_in_background();
    let compaction = crate::work_tracking::spawn_compaction(journal.clone());
    // Gate held: the immediate round ran and was skipped, and nothing folded.
    until("the immediate round is skipped as recovery pending", || {
        skipped.load(Ordering::SeqCst) > 0
    });
    until("the background attempt reaches its gate", || {
        gate.reached.load(Ordering::SeqCst) > 0
    });
    assert!(dead.exists());
    // Gate released: a round runs and folds the dead run's segment, long
    // before the next hourly tick.
    gate.open();
    assert!(journal.wait_recovered(Duration::from_secs(60)));
    until("a compaction round folds the dead run's segment", || {
        !dead.exists()
    });
    compaction.abort();
}
