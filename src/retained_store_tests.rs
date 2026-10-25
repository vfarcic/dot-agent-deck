//! `inbox/recovery/001`–`/012` and `/014`–`/025`: the retained-report store
//! (PRD #1666 part 3, round 5, plan D2, D3, D14).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use spec::spec;

use super::*;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn t0() -> Now {
    Now {
        wall: 1_800_000_000,
        mono: Instant::now(),
    }
}

fn on(base: Now, days: u64) -> Now {
    base.plus(DAY * days as u32)
}

fn config(max_bytes: u64, max_files: u32) -> StoreConfig {
    StoreConfig {
        max_bytes,
        max_files,
        fallback_slots: DEFAULT_FALLBACK_SLOTS,
    }
}

fn open(root: &Path, config: StoreConfig, now: Now) -> Arc<RetainedStore> {
    RetainedStore::open(root, config, now)
}

fn open_armed(
    root: &Path,
    config: StoreConfig,
    now: Now,
    arm: impl FnOnce(&RetainedStore),
) -> Arc<RetainedStore> {
    RetainedStore::open_with(root, config, now, arm)
}

fn envelope(id: &str) -> Envelope {
    Envelope {
        report_id: id.to_string(),
        kind: ArtifactKind::WorkerReport,
        task_id: Some("m-task".into()),
        unit_id: None,
        deck_id: "deck".into(),
        run_id: "run".into(),
        created_at: "2026-10-23T00:00:00Z".into(),
        producer_agent_id: "7".into(),
        recipient_agent_id: "3".into(),
        instance: Some("instance-1".into()),
        producer_supplied: ProducerSupplied {
            unit_name: None,
            role: Some("coder".into()),
            producer_pane_id: "worker-pane".into(),
            recipient_pane_id: "orchestrator-pane".into(),
            orchestration: Some("work".into()),
        },
        truncated: Vec::new(),
    }
}

/// Save `body` as `id` and answer the file's size.
fn put(store: &RetainedStore, id: &str, body: &str, now: Now) -> u64 {
    let contents = compose_file(
        &envelope(id),
        &crate::state::frame_untrusted_report_for_file(body),
    );
    let room = store
        .reserve(contents.len() as u64)
        .expect("room for the report");
    store.write(room, id, &contents, now).expect("saved");
    contents.len() as u64
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

fn files_root(root: &Path) -> PathBuf {
    root.join(ROOT).join(FILES)
}

fn runs_root(root: &Path) -> PathBuf {
    root.join(ROOT).join(RUNS)
}

/// Every retained file under `root`, as (owner run, name).
fn every_file(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for run in names_in(&files_root(root)) {
        for name in names_in(&files_root(root).join(&run)) {
            out.push((run.clone(), name));
        }
    }
    out
}

fn report_of(name: &str) -> String {
    parse_own_name(name)
        .map(str::to_string)
        .or_else(|| parse_adopted_name(name).map(|(id, _, _)| id.to_string()))
        .unwrap_or_else(|| name.to_string())
}

fn counts(store: &RetainedStore) -> StoreCounts {
    store.counts().expect("the store's state")
}

fn size_sum(dir: &Path) -> u64 {
    names_in(dir)
        .iter()
        .map(|n| std::fs::metadata(dir.join(n)).unwrap().len())
        .sum()
}

fn id(n: usize) -> String {
    format!("report-{n:04}")
}

/// Scenario: A store with a 1000-byte budget and two file slots. Reservations within both are admitted; one over the bytes is refused with the byte cause and one over the slots with the file-count cause, consuming nothing; a released reservation frees its room; and a store with one fallback slot admits one dispatch and refuses the next with the slot counts.
#[spec("inbox/recovery/001")]
#[test]
fn recovery_001_admission_refuses_at_the_byte_and_file_bounds() {
    let root = crate::test_temp::tempdir().unwrap();
    let store = open(root.path(), config(1000, 2), t0());
    let a = store.reserve(600).unwrap();
    assert!(matches!(
        store.reserve(500),
        Err(StoreRefusal::Bytes { .. })
    ));
    let b = store.reserve(300).unwrap();
    assert!(matches!(
        store.reserve(1),
        Err(StoreRefusal::Files { max: 2, .. })
    ));
    let c = counts(&store);
    assert_eq!((c.reserved_bytes, c.reserved_slots), (900, 2));
    store.release(a);
    let again = store.reserve(1).unwrap();
    store.release(again);
    store.release(b);
    assert_eq!(counts(&store).reserved_bytes, 0);

    let root = crate::test_temp::tempdir().unwrap();
    let store = open(
        root.path(),
        StoreConfig {
            max_bytes: DEFAULT_MAX_BYTES,
            max_files: 10,
            fallback_slots: 1,
        },
        t0(),
    );
    let first = store.reserve_dispatch().unwrap();
    assert_eq!(counts(&store).reserved_bytes, DISPATCH_RESULT_MAX_FILE);
    assert_eq!(
        store.reserve_dispatch().err(),
        Some(StoreRefusal::Slots {
            in_flight: 1,
            unsaved: 0
        })
    );
    store
        .dispatch_result(first, "dispatch-1", b"started", t0())
        .unwrap();
    assert_eq!(counts(&store).in_flight, 0);
}

/// Scenario: A saved report that stays pinned (no acknowledgement, no recovery) is not removed by a sweep a hundred days later; once its message is acknowledged it is removed a day after.
#[spec("inbox/recovery/002")]
#[test]
fn recovery_002_a_pinned_report_is_never_swept() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let store = open(root.path(), StoreConfig::default(), start);
    put(&store, "pinned", "PINNED-SENTINEL", start);
    store.sweep(on(start, 100));
    assert_eq!(names_in(store.dir()), vec!["r-pinned.md"]);
    assert_eq!(store.own_state("pinned"), Some(OwnState::Pinned));
    let acked = on(start, 100);
    store.acknowledged("pinned", acked);
    store.sweep(acked.plus(DAY - Duration::from_secs(1)));
    assert_eq!(names_in(store.dir()), vec!["r-pinned.md"]);
    store.sweep(acked.plus(DAY));
    assert!(names_in(store.dir()).is_empty());
}

/// Scenario: Expiry from recorded times with an injected clock. An acknowledged report is kept for 24 hours from the acknowledgement and removed at 24 hours; one in recovery is kept for 7 days from its recovery and removed at 7 days; the charge falls with each removal.
#[spec("inbox/recovery/003")]
#[test]
fn recovery_003_expiry_is_measured_from_the_recorded_times() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let store = open(root.path(), StoreConfig::default(), start);
    put(&store, "acked", "a", start);
    put(&store, "recovered", "r", start);
    let at = on(start, 2);
    store.acknowledged("acked", at);
    store.recovered("recovered", at);
    store.sweep(at.plus(ACK_RETENTION - Duration::from_secs(1)));
    assert_eq!(names_in(store.dir()).len(), 2);
    store.sweep(at.plus(ACK_RETENTION));
    assert_eq!(names_in(store.dir()), vec!["r-recovered.md"]);
    store.sweep(at.plus(RECOVERY_RETENTION - Duration::from_secs(1)));
    assert_eq!(names_in(store.dir()), vec!["r-recovered.md"]);
    store.sweep(at.plus(RECOVERY_RETENTION));
    assert!(names_in(store.dir()).is_empty());
    assert_eq!(counts(&store).own_bytes, 0);
    assert_eq!(counts(&store).own_files, 0);
}

/// Scenario: Two runs on one state directory. Run A is live and its files carry a 1970 mtime; run B starts and sweeps. B takes nothing over, charges nothing, and A's files and directory are exactly as they were.
#[spec("inbox/recovery/004")]
#[test]
fn recovery_004_a_live_run_is_never_touched_whatever_its_files_age() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "live-one", "one", start);
    put(&a, "live-two", "two", start);
    a.flush_inventory().unwrap();
    for name in names_in(a.dir()) {
        let file = std::fs::File::options()
            .write(true)
            .open(a.dir().join(&name))
            .unwrap();
        file.set_modified(std::time::UNIX_EPOCH).unwrap();
    }
    let before = names_in(a.dir());
    let b = open(root.path(), StoreConfig::default(), on(start, 30));
    b.sweep(on(start, 30));
    assert!(b.seams.taken.lock().unwrap().is_empty());
    assert_eq!(counts(&b).adopted_bytes, 0);
    assert_eq!(names_in(a.dir()), before);
    drop(a);
}

/// Scenario: A report written on day 0 enters recovery on day 6, its run ends, and the deck restarts on day 8. The new run adopts it with the deadline day 13 in its name and the recovered state letter, keeps it through day 12 and removes it on day 13.
#[spec("inbox/recovery/005")]
#[test]
fn recovery_005_a_delayed_recovery_is_kept_its_seven_days_across_a_restart() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "late", "late recovery", start);
    a.recovered("late", on(start, 6));
    a.flush_inventory().unwrap();
    drop(a);
    let b = open(root.path(), StoreConfig::default(), on(start, 8));
    let deadline = on(start, 13).wall;
    assert_eq!(
        names_in(b.dir()),
        vec![adopted_name("late", deadline, AdoptedState::Recovered)]
    );
    b.sweep(on(start, 12));
    assert_eq!(names_in(b.dir()).len(), 1);
    b.sweep(on(start, 13));
    assert!(names_in(b.dir()).is_empty());
}

/// Scenario: A pinned report's run ends before its message is acknowledged. The next run adopts it pinned with seven days from the takeover; an acknowledgement that arrives in the new run changes nothing, since the new run holds no state for it; a third run keeps the same deadline and letter.
#[spec("inbox/recovery/006")]
#[test]
fn recovery_006_a_delayed_ack_across_a_restart_does_not_shorten_or_renew() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "pending", "unacknowledged", start);
    a.flush_inventory().unwrap();
    drop(a);
    let b = open(root.path(), StoreConfig::default(), on(start, 1));
    let name = adopted_name("pending", on(start, 8).wall, AdoptedState::Pinned);
    assert_eq!(names_in(b.dir()), vec![name.clone()]);
    b.acknowledged("pending", on(start, 2));
    b.flush_inventory().unwrap();
    drop(b);
    let c = open(root.path(), StoreConfig::default(), on(start, 3));
    assert_eq!(names_in(c.dir()), vec![name]);
    c.sweep(on(start, 7));
    assert_eq!(names_in(c.dir()).len(), 1);
    c.sweep(on(start, 8));
    assert!(names_in(c.dir()).is_empty());
}

/// Scenario: A run that ended left reports; the next run takes them over. Its adopted charge equals their bytes, a reservation that fits beside them is admitted, and one that would pass the budget with them is refused.
#[spec("inbox/recovery/007")]
#[test]
fn recovery_007_takeover_charges_the_adopted_bytes() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), StoreConfig::default(), start);
    let total = put(&a, "one", "first report", start) + put(&a, "two", "second", start);
    a.flush_inventory().unwrap();
    drop(a);
    let b = open(root.path(), config(total + 10, 100), on(start, 1));
    assert_eq!(counts(&b).adopted_bytes, total);
    assert!(matches!(b.reserve(11), Err(StoreRefusal::Bytes { .. })));
    let fits = b.reserve(10).unwrap();
    b.release(fits);
}

/// Scenario: Reservations taken concurrently from eight threads are all counted. A report whose removal fails stays charged, with its slot, as an orphaned attempt, and a later sweep with the fault lifted removes it and frees the charge.
#[spec("inbox/recovery/008")]
#[test]
fn recovery_008_concurrent_reservations_count_and_a_failed_unlink_stays_charged() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let store = open(root.path(), config(1_000_000, 1000), start);
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                (0..25)
                    .map(|_| store.reserve(10).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let held: Vec<Reservation> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    assert_eq!(counts(&store).reserved_slots, 200);
    assert_eq!(counts(&store).reserved_bytes, 2000);
    for room in held {
        store.release(room);
    }
    let size = put(&store, "stuck", "body", start);
    store.acknowledged("stuck", start);
    store.seams.fail_unlink.store(true, Ordering::SeqCst);
    store.sweep(on(start, 2));
    assert_eq!(names_in(store.dir()), vec!["r-stuck.md"]);
    assert_eq!(store.own_state("stuck"), Some(OwnState::OrphanUnlink));
    assert_eq!(counts(&store).own_bytes, size);
    assert_eq!(counts(&store).own_files, 1);
    store.seams.fail_unlink.store(false, Ordering::SeqCst);
    store.sweep(on(start, 3));
    assert!(names_in(store.dir()).is_empty());
    assert_eq!(counts(&store).own_bytes, 0);
}

/// Scenario: A run ends before its inventory listed a report. The next run adopts it with seven days from the takeover written into its name and the unlisted letter; a second restart a day later keeps that name, so its retention is not renewed.
#[spec("inbox/recovery/009")]
#[test]
fn recovery_009_an_unlisted_file_gets_seven_days_once() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "unlisted", "never flushed", start);
    drop(a);
    let b = open(root.path(), StoreConfig::default(), on(start, 1));
    let name = adopted_name("unlisted", on(start, 8).wall, AdoptedState::Unlisted);
    assert_eq!(names_in(b.dir()), vec![name.clone()]);
    drop(b);
    let c = open(root.path(), StoreConfig::default(), on(start, 2));
    assert_eq!(names_in(c.dir()), vec![name]);
}

/// Scenario: The `retained-reports` directory already exists open to other users (0755), as a symlink, or as a regular file. Each is refused, never repaired: the store is unusable, every reservation answers unusable, and nothing is created or changed in its place.
#[spec("inbox/recovery/010")]
#[test]
fn recovery_010_a_hostile_store_directory_is_refused() {
    // Open to other users.
    let root = crate::test_temp::tempdir().unwrap();
    let dir = root.path().join(ROOT);
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = open(root.path(), StoreConfig::default(), t0());
    assert!(store.unusable_reason().is_some());
    assert!(matches!(
        store.reserve(1),
        Err(StoreRefusal::Unusable { .. })
    ));
    assert!(matches!(
        store.reserve_dispatch(),
        Err(StoreRefusal::Unusable { .. })
    ));
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(names_in(&dir).is_empty());

    // A symlink.
    let root = crate::test_temp::tempdir().unwrap();
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.path().join(ROOT)).unwrap();
    let store = open(root.path(), StoreConfig::default(), t0());
    assert!(store.unusable_reason().is_some());
    assert!(names_in(&elsewhere).is_empty());

    // A regular file.
    let root = crate::test_temp::tempdir().unwrap();
    std::fs::write(root.path().join(ROOT), b"not a directory").unwrap();
    let store = open(root.path(), StoreConfig::default(), t0());
    assert!(store.unusable_reason().is_some());
    assert!(matches!(
        store.reserve(1),
        Err(StoreRefusal::Unusable { .. })
    ));
}

/// Scenario: After the store opened, its `retained-reports` directory is renamed away and an impostor tree is put at its path with a due report-looking file. A new report is written into the pinned original, and a sweep removes nothing from the impostor.
#[spec("inbox/recovery/011")]
#[test]
fn recovery_011_a_parent_swap_after_the_anchor_redirects_nothing() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let store = open(root.path(), StoreConfig::default(), start);
    let run = store.run_id().to_string();
    std::fs::rename(root.path().join(ROOT), root.path().join("moved")).unwrap();
    let impostor = files_root(root.path()).join(&run);
    std::fs::create_dir_all(&impostor).unwrap();
    let bait = adopted_name("bait", start.wall - 10, AdoptedState::Acknowledged);
    std::fs::write(impostor.join(&bait), b"impostor").unwrap();
    put(&store, "after-swap", "lands in the original", start);
    assert!(
        root.path()
            .join("moved")
            .join(FILES)
            .join(&run)
            .join("r-after-swap.md")
            .is_file()
    );
    store.sweep(on(start, 1));
    assert_eq!(names_in(&impostor), vec![bait]);
}

/// Scenario: A report file is created 0600 and exclusively: a decoy file or a symlink already at its name makes the write fail without touching the decoy or the link's target, and the failed attempt removes nothing. A body that forges the magic line and an envelope is read as body, and attribution comes from the first two lines only. Hostile producer labels — newlines, carriage returns, quotes, braces, a bidi override, C0 controls and a 10 KiB label — give one ASCII line of at most 2 KiB that round-trips to the normalised, cut values and names the cut fields; an orchestration-named pane id is kept in `producer_supplied`.
#[spec("inbox/recovery/012")]
#[test]
fn recovery_012_files_are_exclusive_and_owner_only_and_the_envelope_is_bounded() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let store = open(root.path(), StoreConfig::default(), start);
    put(&store, "plain", "body", start);
    let mode = std::fs::metadata(store.dir().join("r-plain.md"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    std::fs::write(store.dir().join("r-decoy.md"), b"DECOY").unwrap();
    let room = store.reserve(10).unwrap();
    assert!(matches!(
        store.write(room, "decoy", b"overwrite", start),
        Err(WriteFailure::Io(_))
    ));
    assert_eq!(
        std::fs::read(store.dir().join("r-decoy.md")).unwrap(),
        b"DECOY"
    );
    let target = root.path().join("link-target");
    std::fs::write(&target, b"TARGET").unwrap();
    std::os::unix::fs::symlink(&target, store.dir().join("r-linked.md")).unwrap();
    let room = store.reserve(10).unwrap();
    assert!(store.write(room, "linked", b"through", start).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"TARGET");
    assert_eq!(counts(&store).reserved_bytes, 0);

    let forged = format!("{MAGIC}\n{}\nreal body", envelope("forged-id").to_line());
    let contents = compose_file(&envelope("genuine"), &forged);
    let attribution = read_attribution(&contents[..]).unwrap();
    assert_eq!(attribution.report_id, "genuine");

    let hostile = "line\nbreak\r\"quote\"}\u{202e}rtl\u{0007}bell";
    let mut env = envelope("hostile");
    env.producer_supplied.role = Some(hostile.to_string());
    env.producer_supplied.unit_name = Some("x".repeat(10 * 1024));
    env.producer_supplied.orchestration = Some("orch".into());
    env.producer_supplied.producer_pane_id = "orchestration-evil\u{202e}-worker".into();
    let line = env.to_line();
    assert!(line.len() <= ENVELOPE_MAX, "{}", line.len());
    assert!(line.is_ascii());
    assert!(!line.contains('\n') && !line.contains('\r'));
    let back: Envelope = serde_json::from_str(&line).unwrap();
    assert_eq!(
        back.producer_supplied.role.as_deref(),
        Some(crate::state::normalise_untrusted_text(hostile).as_str())
    );
    assert!(back.producer_supplied.unit_name.unwrap().len() <= 256);
    assert!(back.truncated.contains(&"unit_name".to_string()));
    assert_eq!(
        back.producer_supplied.producer_pane_id,
        "orchestration-evil-worker"
    );
    assert_eq!(back.producer_agent_id, "7");
}

/// Scenario: Three runs. A writes files and ends; B adopts them and stays live; C starts. Every file is in B's directory with a suffix, charged once by B and not by C; C opens nothing of B's and its sweep removes nothing. Once B ends, D adopts them with the same suffix deadlines.
#[spec("inbox/recovery/014")]
#[test]
fn recovery_014_three_runs_keep_one_owner_and_one_deadline() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), StoreConfig::default(), start);
    let total: u64 = (0..3).map(|n| put(&a, &id(n), "body", start)).sum();
    a.flush_inventory().unwrap();
    drop(a);
    let b = open(root.path(), StoreConfig::default(), on(start, 1));
    let adopted = names_in(b.dir());
    assert_eq!(adopted.len(), 3);
    assert!(adopted.iter().all(|n| parse_adopted_name(n).is_some()));
    assert_eq!(counts(&b).adopted_bytes, total);
    let c = open(root.path(), StoreConfig::default(), on(start, 2));
    assert_eq!(counts(&c).adopted_bytes, 0);
    assert!(
        !c.seams
            .taken
            .lock()
            .unwrap()
            .contains(&b.run_id().to_string())
    );
    c.sweep(on(start, 2));
    assert_eq!(names_in(b.dir()), adopted);
    let b_run = b.run_id().to_string();
    drop(b);
    let d = open(root.path(), StoreConfig::default(), on(start, 3));
    assert_eq!(names_in(d.dir()), adopted);
    assert!(!files_root(root.path()).join(b_run).exists());
    drop(c);
}

/// Scenario: A crash mid-transfer, for k = 0, 1 and all of A's n files moved. B's transfer is stopped by a seam after k files and B ends without retiring A; then C and D start at once behind a barrier. Every file ends in exactly one owned directory, the live runs' charges sum to the files' sizes, B's adopted files keep their names, and A's lock and inventory are gone only once its directory is empty.
#[spec("inbox/recovery/015")]
#[test]
fn recovery_015_a_crash_mid_transfer_then_two_starters() {
    for k in [0u32, 1, 3] {
        let root = crate::test_temp::tempdir().unwrap();
        let start = t0();
        let a = open(root.path(), StoreConfig::default(), start);
        for n in 0..3 {
            put(&a, &id(n), &format!("body {n}"), start);
        }
        a.flush_inventory().unwrap();
        let a_run = a.run_id().to_string();
        // Each file's inode, so a takeover that copied instead of renaming
        // shows even where the crash point falls between files.
        #[cfg(unix)]
        let inodes: std::collections::HashMap<String, u64> = names_in(a.dir())
            .iter()
            .map(|n| {
                use std::os::unix::fs::MetadataExt;
                (
                    report_of(n),
                    std::fs::metadata(a.dir().join(n)).unwrap().ino(),
                )
            })
            .collect();
        drop(a);
        let b = open_armed(root.path(), StoreConfig::default(), on(start, 1), |store| {
            store.seams.stop_after.store(k, Ordering::SeqCst);
            store.seams.stop_armed.store(true, Ordering::SeqCst);
        });
        let moved_by_b = names_in(b.dir());
        assert_eq!(moved_by_b.len(), k as usize);
        assert_eq!(
            runs_root(root.path()).join(lock_name(&a_run)).exists(),
            k < 3,
            "A is retired only once its directory is empty"
        );
        drop(b);

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let starters: Vec<_> = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let path = root.path().to_path_buf();
                std::thread::spawn(move || {
                    barrier.wait();
                    open(&path, StoreConfig::default(), on(start, 2))
                })
            })
            .collect();
        let live: Vec<Arc<RetainedStore>> =
            starters.into_iter().map(|t| t.join().unwrap()).collect();

        let files = every_file(root.path());
        let mut reports: Vec<String> = files.iter().map(|(_, n)| report_of(n)).collect();
        reports.sort();
        assert_eq!(reports, vec![id(0), id(1), id(2)], "k={k}: {files:?}");
        let live_runs: Vec<String> = live.iter().map(|s| s.run_id().to_string()).collect();
        assert!(
            files.iter().all(|(run, _)| live_runs.contains(run)),
            "k={k}"
        );
        let charged: u64 = live
            .iter()
            .map(|s| counts(s).adopted_bytes + counts(s).own_bytes)
            .sum();
        let sizes: u64 = live.iter().map(|s| size_sum(s.dir())).sum();
        assert_eq!(charged, sizes, "k={k}");
        for name in &moved_by_b {
            assert!(
                files.iter().any(|(_, n)| n == name),
                "k={k}: B's adopted name {name} kept"
            );
        }
        assert!(!runs_root(root.path()).join(lock_name(&a_run)).exists());
        assert!(!runs_root(root.path()).join(inventory_name(&a_run)).exists());
        assert!(!files_root(root.path()).join(&a_run).exists());
        #[cfg(unix)]
        for (run, name) in &files {
            use std::os::unix::fs::MetadataExt;
            let ino = std::fs::metadata(files_root(root.path()).join(run).join(name))
                .unwrap()
                .ino();
            assert_eq!(
                ino,
                inodes[&report_of(name)],
                "k={k}: {name} must be A's file renamed, not a copy"
            );
        }
    }
}

/// A dead run made by hand: its lock, an inventory (raw bytes, or none),
/// and files in its directory.
fn fake_dead_run(root: &Path, inventory: Option<&[u8]>, files: &[(&str, &[u8])]) -> String {
    let run = mint_run_id().unwrap();
    for dir in [ROOT, &format!("{ROOT}/{RUNS}"), &format!("{ROOT}/{FILES}")] {
        let path = root.join(dir);
        let _ = std::fs::create_dir(&path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(runs_root(root).join(lock_name(&run)), b"").unwrap();
    if let Some(inventory) = inventory {
        std::fs::write(runs_root(root).join(inventory_name(&run)), inventory).unwrap();
    }
    let dir = files_root(root).join(&run);
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (name, body) in files {
        std::fs::write(dir.join(name), body).unwrap();
    }
    run
}

fn inventory_bytes(run: &str, max_files: u32, entries: &[InventoryEntry]) -> Vec<u8> {
    write_inventory_text(run, max_files, entries).into_bytes()
}

/// Scenario: Corrupt or foreign inventories — a wrong version, a symlink, a FIFO, unparsable text, a line over 256 bytes, a header max_files over 65,536, more lines than its count, an invalid name and an unknown state — each leave the dead run's unsuffixed file in place and uncharged, while its suffixed file is still adopted. An unrecognised name in an owned directory is left alone, and the inventory writer given revision n + 1 and then n ends holding n + 1.
#[spec("inbox/recovery/016")]
#[test]
fn recovery_016_invalid_inventories_fail_closed_and_the_writer_is_fenced() {
    let start = t0();
    let future = adopted_name("suffixed", start.wall + 1000, AdoptedState::Pinned);
    let entry =
        |name: &str, state: &str| format!("{{\"n\":\"{name}\",\"c\":1,\"s\":\"{state}\"}}\n");
    type MakeInventory = Box<dyn Fn(&str) -> Vec<u8>>;
    let cases: Vec<(&str, MakeInventory)> = vec![
        (
            "wrong version",
            Box::new(|run| {
                format!("{{\"v\":2,\"run\":\"{run}\",\"max_files\":4,\"count\":0}}\n").into_bytes()
            }),
        ),
        ("unparsable", Box::new(|_| b"not json at all\n".to_vec())),
        (
            "long line",
            Box::new(|run| {
                let mut text =
                    format!("{{\"v\":1,\"run\":\"{run}\",\"max_files\":4,\"count\":1}}\n");
                text.push_str(&"x".repeat(300));
                text.push('\n');
                text.into_bytes()
            }),
        ),
        (
            "header over the hard bound",
            Box::new(|run| {
                format!("{{\"v\":1,\"run\":\"{run}\",\"max_files\":70000,\"count\":0}}\n")
                    .into_bytes()
            }),
        ),
        (
            "more lines than its count",
            Box::new(move |run| {
                let mut text =
                    format!("{{\"v\":1,\"run\":\"{run}\",\"max_files\":4,\"count\":0}}\n");
                text.push_str(&entry("r-unsuffixed.md", "pinned"));
                text.into_bytes()
            }),
        ),
        (
            "invalid name",
            Box::new(move |run| {
                let mut text =
                    format!("{{\"v\":1,\"run\":\"{run}\",\"max_files\":4,\"count\":1}}\n");
                text.push_str(&entry("../escape.md", "pinned"));
                text.into_bytes()
            }),
        ),
        (
            "unknown state",
            Box::new(move |run| {
                let mut text =
                    format!("{{\"v\":1,\"run\":\"{run}\",\"max_files\":4,\"count\":1}}\n");
                text.push_str(&entry("r-unsuffixed.md", "glorious"));
                text.into_bytes()
            }),
        ),
    ];
    for (what, make) in cases {
        let root = crate::test_temp::tempdir().unwrap();
        let run = fake_dead_run(
            root.path(),
            None,
            &[("r-unsuffixed.md", b"unsuffixed"), (&future, b"suffixed")],
        );
        std::fs::write(
            runs_root(root.path()).join(inventory_name(&run)),
            make(&run),
        )
        .unwrap();
        let store = open(root.path(), StoreConfig::default(), start);
        assert_eq!(names_in(store.dir()), vec![future.clone()], "{what}");
        assert_eq!(counts(&store).adopted_bytes, 8, "{what}");
        assert_eq!(
            names_in(&files_root(root.path()).join(&run)),
            vec!["r-unsuffixed.md"],
            "{what}"
        );
    }
    for special in ["symlink", "fifo"] {
        let root = crate::test_temp::tempdir().unwrap();
        let run = fake_dead_run(root.path(), None, &[("r-unsuffixed.md", b"unsuffixed")]);
        let at = runs_root(root.path()).join(inventory_name(&run));
        if special == "symlink" {
            let real = root.path().join("real.json");
            std::fs::write(&real, inventory_bytes(&run, 4, &[])).unwrap();
            std::os::unix::fs::symlink(&real, &at).unwrap();
        } else {
            let c = std::ffi::CString::new(at.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a valid path and mode.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        }
        let store = open(root.path(), StoreConfig::default(), start);
        assert!(names_in(store.dir()).is_empty(), "{special}");
        assert_eq!(
            names_in(&files_root(root.path()).join(&run)),
            vec!["r-unsuffixed.md"],
            "{special}"
        );
    }

    let root = crate::test_temp::tempdir().unwrap();
    let store = open(root.path(), StoreConfig::default(), start);
    std::fs::write(store.dir().join("garbage.txt"), b"left alone").unwrap();
    store.sweep(on(start, 30));
    assert_eq!(names_in(store.dir()), vec!["garbage.txt"]);

    assert!(store.write_inventory(5, "newer\n").unwrap());
    assert!(!store.write_inventory(4, "older\n").unwrap());
    assert_eq!(
        std::fs::read_to_string(runs_root(root.path()).join(inventory_name(store.run_id())))
            .unwrap(),
        "newer\n"
    );
}

/// Scenario: A deck-authored dispatch result whose whole file is exactly the 72 KiB limit is saved whole, with no cut note; one byte more is cut, carries the note, and stays within the limit; and a dispatch reserves exactly that limit.
#[spec("inbox/recovery/017")]
#[test]
fn recovery_017_a_dispatch_result_fits_its_file_limit() {
    let env = envelope("dispatch-result");
    let base = compose_dispatch_result(&env, "x").len() - 1;
    let limit = DISPATCH_RESULT_MAX_FILE as usize;
    let exact = "x".repeat(limit - base);
    let file = compose_dispatch_result(&env, &exact);
    assert_eq!(file.len(), limit);
    assert!(!String::from_utf8_lossy(&file).contains(DISPATCH_CUT_NOTE));
    let over = "x".repeat(limit - base + 1);
    let file = compose_dispatch_result(&env, &over);
    assert!(file.len() <= limit);
    assert!(String::from_utf8_lossy(&file).contains(DISPATCH_CUT_NOTE));

    let root = crate::test_temp::tempdir().unwrap();
    let store = open(root.path(), StoreConfig::default(), t0());
    let reserved = store.reserve_dispatch().unwrap();
    assert_eq!(counts(&store).reserved_bytes, DISPATCH_RESULT_MAX_FILE);
    let _ = store.dispatch_result(reserved, "dispatch-x", &file, t0());
}

/// Scenario: A filesystem without a no-replace rename (an injected ENOSYS on every move). The takeover of a dead run stops: its files stay where they were, uncharged, and the run is not retired, with no overwrite and no second name.
#[spec("inbox/recovery/018")]
#[test]
fn recovery_018_an_unsupported_rename_stops_the_transfer() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "one", "1", start);
    put(&a, "two", "2", start);
    a.flush_inventory().unwrap();
    let a_dir = a.dir().to_path_buf();
    let a_run = a.run_id().to_string();
    drop(a);
    let b = open_armed(root.path(), StoreConfig::default(), on(start, 1), |store| {
        store.seams.move_errno.store(libc::ENOSYS, Ordering::SeqCst);
    });
    assert!(names_in(b.dir()).is_empty());
    assert_eq!(names_in(&a_dir), vec!["r-one.md", "r-two.md"]);
    assert_eq!(counts(&b).adopted_bytes, 0);
    assert!(runs_root(root.path()).join(lock_name(&a_run)).exists());
}

/// Scenario: With MAX_FILES 64, 64 tiny reports with maximum-length ids are accepted and the 65th is refused with the file-count cause though the bytes are far below the budget. A new run with MAX_FILES 8 reads the predecessor's inventory from its header, adopts all 64 and charges each one's size; a second restart keeps their names and charges each once. An inventory of 65,536 entries with maximum-length names and every optional field is read back whole, every line within 256 bytes, and the writer refuses a count over its max_files.
#[spec("inbox/recovery/019")]
#[test]
fn recovery_019_the_inventory_writer_and_reader_agree_at_the_boundary() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), config(DEFAULT_MAX_BYTES, 64), start);
    let long_id = |n: usize| format!("{n:04}{}", "z".repeat(60));
    for n in 0..64 {
        put(&a, &long_id(n), "t", start);
    }
    let contents = compose_file(&envelope(&long_id(64)), "t");
    assert!(matches!(
        a.reserve(contents.len() as u64),
        Err(StoreRefusal::Files { max: 64, .. })
    ));
    a.flush_inventory().unwrap();
    let total = size_sum(a.dir());
    drop(a);
    let b = open(root.path(), config(DEFAULT_MAX_BYTES, 8), on(start, 1));
    let names = names_in(b.dir());
    assert_eq!(names.len(), 64);
    assert_eq!(counts(&b).adopted_bytes, total);
    let deadline = on(start, 8).wall;
    assert!(
        names
            .iter()
            .all(|n| parse_adopted_name(n).unwrap().1 == deadline)
    );
    drop(b);
    let c = open(root.path(), config(DEFAULT_MAX_BYTES, 8), on(start, 2));
    assert_eq!(names_in(c.dir()), names);
    assert_eq!(counts(&c).adopted_bytes, total);

    let entries: Vec<InventoryEntry> = (0..HARD_MAX_FILES as usize)
        .map(|n| InventoryEntry {
            name: own_name(&format!("{n:064}")),
            created_at: i64::MAX / 2,
            state: OwnState::Recovered(i64::MAX / 2),
        })
        .collect();
    let text = write_inventory_text("run-x", HARD_MAX_FILES, &entries);
    assert!(text.lines().all(|l| l.len() <= INVENTORY_LINE_MAX));
    let (max, back) = read_inventory(text.as_bytes(), "run-x").unwrap();
    assert_eq!((max, back.len()), (HARD_MAX_FILES, HARD_MAX_FILES as usize));
    assert!(
        std::panic::catch_unwind(|| write_inventory_text("run-x", 1, &entries[..2])).is_err(),
        "the writer refuses a count over its max_files"
    );
}

/// Scenario: Durability and dead-source cleanup. A failed parent sync of this run's new directory leaves the store unusable. A failed sync of a dead run's directory after the moves leaves it unretired, its lock and inventory in place, and the next start retires it. An expired file whose unlink fails in the dead run's directory is moved here under its past suffix, charged but holding no slot and in no inventory, and removed by the next sweep once the unlink succeeds; with the move failing too, it stays and the dead run is not retired.
#[spec("inbox/recovery/020")]
#[test]
fn recovery_020_syncs_gate_retirement_and_failed_source_unlinks_are_taken_over() {
    let start = t0();
    let root = crate::test_temp::tempdir().unwrap();
    let store = open_armed(root.path(), StoreConfig::default(), start, |store| {
        store.seams.failing_syncs.lock().unwrap().insert("files");
    });
    assert!(matches!(
        store.reserve(1),
        Err(StoreRefusal::Unusable { .. })
    ));

    let root = crate::test_temp::tempdir().unwrap();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "kept", "k", start);
    a.flush_inventory().unwrap();
    let a_run = a.run_id().to_string();
    drop(a);
    let b = open_armed(root.path(), StoreConfig::default(), on(start, 1), |store| {
        store.seams.failing_syncs.lock().unwrap().insert("theirs");
    });
    assert_eq!(names_in(b.dir()).len(), 1);
    assert!(runs_root(root.path()).join(lock_name(&a_run)).exists());
    assert!(runs_root(root.path()).join(inventory_name(&a_run)).exists());
    drop(b);
    let _c = open(root.path(), StoreConfig::default(), on(start, 2));
    assert!(!runs_root(root.path()).join(lock_name(&a_run)).exists());

    let root = crate::test_temp::tempdir().unwrap();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "expired", "e", start);
    a.acknowledged("expired", start);
    a.flush_inventory().unwrap();
    drop(a);
    let b = open_armed(
        root.path(),
        config(DEFAULT_MAX_BYTES, 1),
        on(start, 2),
        |store| {
            store.seams.fail_unlink.store(true, Ordering::SeqCst);
        },
    );
    let past = adopted_name(
        "expired",
        start.wall + secs(ACK_RETENTION),
        AdoptedState::Acknowledged,
    );
    assert_eq!(names_in(b.dir()), vec![past]);
    assert!(counts(&b).adopted_bytes > 0);
    assert_eq!(counts(&b).own_files, 0);
    let slot = b.reserve(1).unwrap();
    b.release(slot);
    b.flush_inventory().unwrap();
    let inventory = std::fs::read(runs_root(root.path()).join(inventory_name(b.run_id()))).unwrap();
    assert_eq!(
        read_inventory(&inventory[..], b.run_id()).unwrap().1.len(),
        0
    );
    b.seams.fail_unlink.store(false, Ordering::SeqCst);
    b.sweep(on(start, 2));
    assert!(names_in(b.dir()).is_empty());
    assert_eq!(counts(&b).adopted_bytes, 0);

    let root = crate::test_temp::tempdir().unwrap();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "stays", "s", start);
    a.acknowledged("stays", start);
    a.flush_inventory().unwrap();
    let (a_dir, a_run) = (a.dir().to_path_buf(), a.run_id().to_string());
    drop(a);
    let b = open_armed(root.path(), StoreConfig::default(), on(start, 2), |store| {
        store.seams.fail_unlink.store(true, Ordering::SeqCst);
        store.seams.move_errno.store(libc::EIO, Ordering::SeqCst);
    });
    assert!(names_in(b.dir()).is_empty());
    assert_eq!(names_in(&a_dir), vec!["r-stays.md"]);
    assert!(runs_root(root.path()).join(lock_name(&a_run)).exists());
}

/// Run `n` reports through a run with `max_files`, acknowledge them all at
/// `start`, flush and end it; answer its run id.
fn ended_run_of_acked(root: &Path, max_files: u32, n: usize, prefix: &str, start: Now) -> String {
    let a = open(root, config(DEFAULT_MAX_BYTES, max_files), start);
    for i in 0..n {
        let id = format!("{prefix}-{i:03}");
        put(&a, &id, "x", start);
        a.acknowledged(&id, start);
    }
    a.flush_inventory().unwrap();
    let run = a.run_id().to_string();
    drop(a);
    run
}

/// Scenario: Inherited cleanup never enters the inventory. A 64-file run's files expire and it ends; B (MAX_FILES 8) takes them over with every source unlink failing, so all 64 land in B's directory under past suffixes, charged and uninventoried; B admits 8 own files and refuses the 9th, and its inventory is valid. B ends with the unlinks still failing; C adopts B's 8 own and 64 inherited files, keeps the inherited deadlines, and charges their sizes; with the fault lifted C's sweep removes the 64. Two dead runs of 64 such files each are taken over by F with no slot used and a valid inventory.
#[spec("inbox/recovery/021")]
#[test]
fn recovery_021_inherited_cleanup_never_enters_the_inventory() {
    let start = t0();
    let root = crate::test_temp::tempdir().unwrap();
    ended_run_of_acked(root.path(), 64, 64, "a", start);
    let failing = |store: &RetainedStore| store.seams.fail_unlink.store(true, Ordering::SeqCst);
    let b = open_armed(
        root.path(),
        config(DEFAULT_MAX_BYTES, 8),
        on(start, 2),
        failing,
    );
    assert_eq!(names_in(b.dir()).len(), 64);
    let inherited = size_sum(b.dir());
    assert_eq!(counts(&b).adopted_bytes, inherited);
    for i in 0..8 {
        put(&b, &format!("b-{i}"), "own", on(start, 2));
    }
    let contents = compose_file(&envelope("b-9"), "own");
    assert!(matches!(
        b.reserve(contents.len() as u64),
        Err(StoreRefusal::Files { max: 8, .. })
    ));
    b.flush_inventory().unwrap();
    let text = std::fs::read(runs_root(root.path()).join(inventory_name(b.run_id()))).unwrap();
    let (max, entries) = read_inventory(&text[..], b.run_id()).unwrap();
    assert_eq!((max, entries.len()), (8, 8));
    drop(b);

    let c = open_armed(
        root.path(),
        config(DEFAULT_MAX_BYTES, 8),
        on(start, 3),
        failing,
    );
    let names = names_in(c.dir());
    assert_eq!(names.len(), 72);
    let past = start.wall + secs(ACK_RETENTION);
    assert_eq!(
        names
            .iter()
            .filter(|n| parse_adopted_name(n).unwrap().1 == past)
            .count(),
        64
    );
    assert_eq!(counts(&c).adopted_bytes, size_sum(c.dir()));
    c.flush_inventory().unwrap();
    let text = std::fs::read(runs_root(root.path()).join(inventory_name(c.run_id()))).unwrap();
    assert_eq!(read_inventory(&text[..], c.run_id()).unwrap().1.len(), 0);
    let before = counts(&c);
    c.seams.fail_unlink.store(false, Ordering::SeqCst);
    c.sweep(on(start, 3));
    assert_eq!(names_in(c.dir()).len(), 8);
    assert_eq!(counts(&c).adopted_bytes, size_sum(c.dir()));
    assert_eq!(counts(&c).own_files, before.own_files);
    drop(c);

    let root = crate::test_temp::tempdir().unwrap();
    ended_run_of_acked(root.path(), 64, 64, "d", start);
    ended_run_of_acked(root.path(), 64, 64, "e", start);
    let f = open_armed(
        root.path(),
        config(DEFAULT_MAX_BYTES, 8),
        on(start, 2),
        failing,
    );
    assert_eq!(names_in(f.dir()).len(), 128);
    assert_eq!(counts(&f).own_files, 0);
    assert_eq!(counts(&f).adopted_bytes, size_sum(f.dir()));
    f.flush_inventory().unwrap();
    let text = std::fs::read(runs_root(root.path()).join(inventory_name(f.run_id()))).unwrap();
    assert!(read_inventory(&text[..], f.run_id()).unwrap().1.is_empty());
}

/// A run holding `n` adopted files with deadlines a week out.
fn with_adopted(root: &Path, n: usize, start: Now, config: StoreConfig) -> Arc<RetainedStore> {
    let a = open(root, StoreConfig::default(), start);
    for i in 0..n {
        put(&a, &id(i), &format!("adopted body {i}"), start);
    }
    a.flush_inventory().unwrap();
    drop(a);
    open(root, config, on(start, 1))
}

/// The bytes `n` adopted reports take, measured through a throwaway run.
fn adopted_size(n: usize, start: Now) -> u64 {
    let root = crate::test_temp::tempdir().unwrap();
    let probe = with_adopted(root.path(), n, start, StoreConfig::default());
    counts(&probe).adopted_bytes
}

/// Scenario: The adopted sum follows the directory, conservatively. Three adopted files deleted from outside are uncharged by the next complete sweep and a refused reservation is then admitted; an own file deleted from outside is released. A seeded sum near u64::MAX saturates on increment. An fstatat EIO on one entry, or a stream that fails after some entries, or a generation moved mid-pass, discards the recount and keeps the stored sum, and the next complete pass publishes it. An own file failing with EACCES keeps its charge. Two sweeps requested together do not overlap. Takeover gives each source state its letter, a second takeover keeps letter and deadline, and a name with an unknown letter or none is left alone, uncharged. A 300-byte inventory line is refused with the reader's buffer never above 257 bytes, and trailing bytes, a duplicate name and a suffixed name make an inventory invalid.
#[spec("inbox/recovery/022")]
#[test]
fn recovery_022_the_adopted_sum_follows_the_directory_conservatively() {
    let start = t0();
    let root = crate::test_temp::tempdir().unwrap();
    let full = adopted_size(6, start);
    let store = with_adopted(root.path(), 6, start, config(full + 100, 100));
    assert_eq!(counts(&store).adopted_bytes, full);
    assert!(matches!(
        store.reserve(150),
        Err(StoreRefusal::Bytes { .. })
    ));
    let names = names_in(store.dir());

    // An EIO on one entry mid-pass, after three deletions: the stored sum stays.
    let deleted: u64 = names[..3]
        .iter()
        .map(|n| {
            let len = std::fs::metadata(store.dir().join(n)).unwrap().len();
            std::fs::remove_file(store.dir().join(n)).unwrap();
            len
        })
        .sum();
    store
        .seams
        .fstat_faults
        .lock()
        .unwrap()
        .insert(names[4].clone(), libc::EIO);
    assert!(!store.sweep(on(start, 1)).recounted);
    assert_eq!(counts(&store).adopted_bytes, full);
    // A stream failing after one entry: the same.
    store.seams.fstat_faults.lock().unwrap().clear();
    store.seams.stream_fail_at.store(1, Ordering::SeqCst);
    store.seams.stream_fail_armed.store(true, Ordering::SeqCst);
    assert!(!store.sweep(on(start, 1)).recounted);
    assert_eq!(counts(&store).adopted_bytes, full);
    store.seams.stream_fail_armed.store(false, Ordering::SeqCst);
    // A generation moved mid-pass: the same.
    store.seams.bump_mid_pass.store(true, Ordering::SeqCst);
    assert!(!store.sweep(on(start, 1)).recounted);
    assert_eq!(counts(&store).adopted_bytes, full);
    // A complete pass publishes the lower sum.
    assert!(store.sweep(on(start, 1)).recounted);
    assert_eq!(counts(&store).adopted_bytes, full - deleted);
    let admitted = store
        .reserve(150)
        .expect("admitted once the deletions are uncharged");
    store.release(admitted);

    // A failed unlink subtracts nothing, and the next recount still charges it.
    let due_root = crate::test_temp::tempdir().unwrap();
    let due = with_adopted(due_root.path(), 2, start, StoreConfig::default());
    due.seams.fail_unlink.store(true, Ordering::SeqCst);
    let before = counts(&due).adopted_bytes;
    due.sweep(on(start, 30));
    assert_eq!(counts(&due).adopted_bytes, before);
    assert_eq!(names_in(due.dir()).len(), 2);

    // Own files: EACCES keeps the charge, ENOENT releases it.
    let own_root = crate::test_temp::tempdir().unwrap();
    let own = open(own_root.path(), StoreConfig::default(), start);
    put(&own, "kept", "k", start);
    put(&own, "gone", "g", start);
    own.seams
        .fstat_faults
        .lock()
        .unwrap()
        .insert(own_name("kept"), libc::EACCES);
    std::fs::remove_file(own.dir().join(own_name("gone"))).unwrap();
    own.sweep(start);
    assert_eq!(own.own_state("kept"), Some(OwnState::Pinned));
    assert_eq!(own.own_state("gone"), None);
    assert_eq!(counts(&own).own_files, 1);

    // Saturation.
    own.state.try_lock().unwrap().adopted_bytes = u64::MAX - 1;
    own.state.try_lock().unwrap().adopt(10);
    assert_eq!(counts(&own).adopted_bytes, u64::MAX);

    // Two sweeps at once run one after the other.
    let a = Arc::clone(&store);
    let b = Arc::clone(&store);
    let ta = std::thread::spawn(move || a.sweep(on(start, 1)));
    let tb = std::thread::spawn(move || b.sweep(on(start, 1)));
    ta.join().unwrap();
    tb.join().unwrap();
    assert_eq!(store.seams.most_active.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&store).adopted_bytes, size_sum(store.dir()));

    // State letters through two takeovers.
    let letters_root = crate::test_temp::tempdir().unwrap();
    let a = open(letters_root.path(), StoreConfig::default(), start);
    put(&a, "acked", "a", start);
    put(&a, "pinned", "p", start);
    put(&a, "recovered", "r", start);
    a.acknowledged("acked", start);
    a.recovered("recovered", start);
    a.flush_inventory().unwrap();
    put(&a, "unlisted", "u", start);
    // A saved file whose removal failed: an orphan under its own name.
    put(&a, "orphan", "o", start);
    a.seams.fail_unlink.store(true, Ordering::SeqCst);
    a.discard("orphan");
    a.seams.fail_unlink.store(false, Ordering::SeqCst);
    assert_eq!(a.own_state("orphan"), Some(OwnState::OrphanUnlink));
    // List the orphan, but not `unlisted`'s later state.
    let entries: Vec<InventoryEntry> = a
        .state
        .try_lock()
        .unwrap()
        .entries()
        .into_iter()
        .filter(|e| e.name != own_name("unlisted"))
        .collect();
    a.write_inventory(1000, &write_inventory_text(a.run_id(), 4096, &entries))
        .unwrap();
    drop(a);
    let b = open(letters_root.path(), StoreConfig::default(), start);
    let mut letters: Vec<(String, char)> = names_in(b.dir())
        .iter()
        .map(|n| {
            let (id, _, state) = parse_adopted_name(n).unwrap();
            (id.to_string(), state.letter())
        })
        .collect();
    letters.sort();
    assert_eq!(
        letters,
        vec![
            ("acked".into(), 'a'),
            ("orphan".into(), 'o'),
            ("pinned".into(), 'p'),
            ("recovered".into(), 'r'),
            ("unlisted".into(), 'u'),
        ]
    );
    let first = names_in(b.dir());
    std::fs::write(b.dir().join("r-odd.d99999999999.x.md"), b"odd").unwrap();
    std::fs::write(b.dir().join("r-bare.d99999999999.md"), b"bare").unwrap();
    drop(b);
    let c = open(
        letters_root.path(),
        StoreConfig::default(),
        start.plus(Duration::from_secs(3600)),
    );
    assert_eq!(names_in(c.dir()), first);
    assert_eq!(counts(&c).adopted_bytes, size_sum(c.dir()));

    // The reader's bounds.
    let mut long = "{\"v\":1,\"run\":\"r\",\"max_files\":4,\"count\":1}\n".to_string();
    long.push_str(&"y".repeat(300));
    long.push('\n');
    let mut lines = BoundedLines::new(long.as_bytes(), INVENTORY_LINE_MAX);
    lines.next_line().unwrap();
    assert!(matches!(lines.next_line(), Err(LineError::TooLong)));
    assert!(lines.peak <= INVENTORY_LINE_MAX);
    // A line of exactly the bound is read; one byte more is refused, and the
    // buffer never grows past the bound.
    let at_bound = format!("{}\n{}\n", "a".repeat(16), "b".repeat(17));
    let mut lines = BoundedLines::new(at_bound.as_bytes(), 16);
    assert_eq!(lines.next_line().unwrap().unwrap().len(), 16);
    assert!(matches!(lines.next_line(), Err(LineError::TooLong)));
    assert!(lines.peak <= 16);
    let header = "{\"v\":1,\"run\":\"r\",\"max_files\":4,\"count\":1}\n";
    for bad in [
        format!("{header}{{\"n\":\"r-a.md\",\"c\":1,\"s\":\"pinned\"}}\ntrailing"),
        "{\"v\":1,\"run\":\"r\",\"max_files\":4,\"count\":2}\n{\"n\":\"r-a.md\",\"c\":1,\"s\":\"pinned\"}\n{\"n\":\"r-a.md\",\"c\":1,\"s\":\"pinned\"}\n"
            .to_string(),
        format!("{header}{{\"n\":\"r-a.d5.p.md\",\"c\":1,\"s\":\"pinned\"}}\n"),
    ] {
        assert!(read_inventory(bad.as_bytes(), "r").is_err(), "{bad}");
    }
}

/// An attempt's temporary names in `dir`.
fn temps_in(dir: &Path) -> Vec<String> {
    names_in(dir)
        .into_iter()
        .filter(|name| is_temp_name(name))
        .collect()
}

/// Scenario: A write whose new file cannot be made owner-only fails and leaves nothing untracked, so a retry with the same report id is saved; when the file's own removal fails too, the store removes it, or keeps it as a charged stray that the next sweep removes, and even a zero-byte write's stray charges a byte. A write that fails and whose temporary file cannot be removed reports an orphaned attempt: the temporary file stays charged its room without taking a file slot or blocking the same id, the next sweep removes it once removals work, and one left when the run ends is removed by the next run's takeover, which retires the run.
#[spec("inbox/recovery/023")]
#[test]
fn recovery_023_a_failed_write_leaves_nothing_untracked() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let store = open(root.path(), StoreConfig::default(), start);

    crate::platform::trusted_dir::test_seams::fail_next_owner_only();
    let room = store.reserve(64).unwrap();
    assert!(matches!(
        store.write(room, "perm", b"body", start),
        Err(WriteFailure::Io(_))
    ));
    assert_eq!(names_in(store.dir()), Vec::<String>::new());
    assert_eq!(counts(&store), StoreCounts::default());
    put(&store, "perm", "retried", start);
    assert_eq!(names_in(store.dir()), vec!["r-perm.md"]);

    // The owner-only step fails and so does its own removal of the file: the
    // store removes it instead, or keeps it as a charged stray that the next
    // sweep removes once removals work.
    for unlink_fails in [false, true] {
        crate::platform::trusted_dir::test_seams::fail_next_owner_only();
        crate::platform::trusted_dir::test_seams::fail_next_rollback();
        store
            .seams
            .fail_unlink
            .store(unlink_fails, Ordering::SeqCst);
        let room = store.reserve(64).unwrap();
        let failed = store.write(room, "both", b"body", start);
        store.seams.fail_unlink.store(false, Ordering::SeqCst);
        let left = temps_in(store.dir());
        let held = counts(&store);
        assert_eq!(held.reserved_bytes, 0);
        assert_eq!(store.own_state("both"), None);
        if unlink_fails {
            assert!(
                matches!(failed, Err(WriteFailure::IoOrphaned(_))),
                "{failed:?}"
            );
            assert_eq!(left.len(), 1, "{:?}", names_in(store.dir()));
            assert!(left[0].starts_with("w-both."));
            assert_eq!((held.stray_files, held.stray_bytes), (1, 64));
            assert_eq!(store.sweep(start).removed, 1);
        } else {
            assert!(matches!(failed, Err(WriteFailure::Io(_))), "{failed:?}");
        }
        assert_eq!(temps_in(store.dir()), Vec::<String>::new());
        assert_eq!(names_in(store.dir()), vec!["r-perm.md"]);
        assert_eq!(
            (counts(&store).stray_files, counts(&store).stray_bytes),
            (0, 0)
        );
    }

    let room = store.reserve(64).unwrap();
    store.seams.fail_unlink.store(true, Ordering::SeqCst);
    store.seams.fail_write.store(true, Ordering::SeqCst);
    assert!(matches!(
        store.write(room, "stray", b"body", start),
        Err(WriteFailure::IoOrphaned(_))
    ));
    store.seams.fail_unlink.store(false, Ordering::SeqCst);
    let stray = temps_in(store.dir());
    assert_eq!(stray.len(), 1, "{:?}", names_in(store.dir()));
    assert!(stray[0].starts_with("w-stray."));
    assert_eq!(store.own_state("stray"), None);
    let held = counts(&store);
    assert_eq!((held.stray_files, held.stray_bytes), (1, 64));
    assert_eq!(held.own_files, 1, "a stray takes no file slot");
    assert_eq!(held.reserved_bytes, 0);
    let tight_root = crate::test_temp::tempdir().unwrap();
    let tight = open(tight_root.path(), config(64, 100), start);
    let room = tight.reserve(64).unwrap();
    tight.seams.fail_unlink.store(true, Ordering::SeqCst);
    tight.seams.fail_write.store(true, Ordering::SeqCst);
    assert!(tight.write(room, "full", b"body", start).is_err());
    tight.seams.fail_unlink.store(false, Ordering::SeqCst);
    assert!(
        matches!(tight.reserve(1), Err(StoreRefusal::Bytes { .. })),
        "a stray's room stays charged"
    );
    drop(tight);
    // A zero-byte write's stray still charges a byte, so strays are bounded.
    let zero_root = crate::test_temp::tempdir().unwrap();
    let zero = open(zero_root.path(), config(1, 100), start);
    let room = zero.reserve(0).unwrap();
    zero.seams.fail_unlink.store(true, Ordering::SeqCst);
    zero.seams.fail_write.store(true, Ordering::SeqCst);
    assert!(zero.write(room, "empty", b"", start).is_err());
    zero.seams.fail_unlink.store(false, Ordering::SeqCst);
    assert_eq!(
        (counts(&zero).stray_files, counts(&zero).stray_bytes),
        (1, 1)
    );
    assert!(
        matches!(zero.reserve(0), Err(StoreRefusal::Bytes { .. })),
        "a zero-byte stray is charged"
    );
    drop(zero);
    put(&store, "stray", "the same id again", start);
    assert_eq!(store.own_state("stray"), Some(OwnState::Pinned));
    let swept = store.sweep(start);
    assert_eq!(swept.removed, 1);
    assert_eq!(temps_in(store.dir()), Vec::<String>::new());
    assert_eq!(
        (counts(&store).stray_files, counts(&store).stray_bytes),
        (0, 0)
    );

    // A stray the run ends with is the next run's to remove.
    let room = store.reserve(64).unwrap();
    store.seams.fail_unlink.store(true, Ordering::SeqCst);
    store.seams.fail_write.store(true, Ordering::SeqCst);
    assert!(store.write(room, "left", b"body", start).is_err());
    store.seams.fail_unlink.store(false, Ordering::SeqCst);
    store.flush_inventory().unwrap();
    let run = store.run_id().to_string();
    drop(store);
    let next = open(root.path(), StoreConfig::default(), on(start, 1));
    let mut adopted: Vec<String> = names_in(next.dir()).iter().map(|n| report_of(n)).collect();
    adopted.sort();
    assert_eq!(adopted, vec!["perm", "stray"]);
    assert_eq!(temps_in(next.dir()), Vec::<String>::new());
    assert!(
        !files_root(root.path()).join(&run).exists(),
        "the run is retired"
    );
    assert_eq!(counts(&next).adopted_bytes, size_sum(next.dir()));
}

/// Scenario: A report in recovery comes due and a sweep nominates it, then is held before it decides; an acknowledgement arrives and renews it, and the sweep keeps it, removing it only a day later. Another sweep is held after it claimed a due report and before removing it; an acknowledgement then renews nothing, and the report is removed.
#[spec("inbox/recovery/024")]
#[test]
fn recovery_024_a_sweep_decides_under_the_lock() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let store = open(root.path(), StoreConfig::default(), start);
    put(&store, "late", "renewed while the sweep looked", start);
    store.recovered("late", start);
    let due = on(start, 7);
    let pause = store.seams.pause_at(seams::Pause::BeforeClaim);
    let sweeper = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || store.sweep(due))
    };
    pause.wait();
    store.acknowledged("late", due);
    pause.wait();
    let report = sweeper.join().unwrap();
    assert_eq!(report.removed, 0, "{report:?}");
    assert_eq!(names_in(store.dir()), vec!["r-late.md"]);
    assert!(matches!(
        store.own_state("late"),
        Some(OwnState::Acknowledged(_))
    ));
    assert_eq!(
        store.sweep(due.plus(DAY - Duration::from_secs(1))).removed,
        0
    );
    assert_eq!(store.sweep(due.plus(DAY)).removed, 1);
    assert_eq!(names_in(store.dir()), Vec::<String>::new());

    put(&store, "claimed", "removed once claimed", start);
    store.acknowledged("claimed", start);
    let due = on(start, 1);
    let pause = store.seams.pause_at(seams::Pause::BeforeUnlink);
    let sweeper = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || store.sweep(due))
    };
    pause.wait();
    assert_eq!(store.own_state("claimed"), Some(OwnState::OrphanUnlink));
    store.acknowledged("claimed", due);
    assert_eq!(store.own_state("claimed"), Some(OwnState::OrphanUnlink));
    pause.wait();
    assert_eq!(sweeper.join().unwrap().removed, 1);
    assert_eq!(store.own_state("claimed"), None);
    assert_eq!(names_in(store.dir()), Vec::<String>::new());
    assert_eq!(counts(&store).own_bytes, 0);
}

/// Scenario: A run saves one report whole, then dies halfway through writing a second, and, on a filesystem without a no-replace rename, dies again just after linking a third to its name. The next run adopts the whole report and the linked one, each once and with its whole contents, adopts nothing of the cut-off write, removes both temporary files and retires the dead run.
#[spec("inbox/recovery/025")]
#[test]
fn recovery_025_a_crash_mid_write_leaves_no_cut_off_report() {
    let root = crate::test_temp::tempdir().unwrap();
    let start = t0();
    let a = open(root.path(), StoreConfig::default(), start);
    put(&a, "whole", "saved before the crash", start);
    a.flush_inventory().unwrap();
    let contents = |id: &str| {
        compose_file(
            &envelope(id),
            &crate::state::frame_untrusted_report_for_file("a report long enough to cut in half"),
        )
    };

    let cut = contents("cut");
    let room = a.reserve(cut.len() as u64).unwrap();
    a.seams
        .crash
        .store(seams::Crash::MidWrite as u8, Ordering::SeqCst);
    assert!(a.write(room, "cut", &cut, start).is_err());
    let temps = temps_in(a.dir());
    assert_eq!(temps.len(), 1);
    assert_eq!(
        std::fs::metadata(a.dir().join(&temps[0])).unwrap().len(),
        (cut.len() / 2) as u64,
        "the crash left a cut-off file"
    );
    assert!(!a.dir().join(own_name("cut")).exists());

    // Where the filesystem has no no-replace rename, the write links instead.
    let linked = contents("linked");
    let room = a.reserve(linked.len() as u64).unwrap();
    crate::platform::trusted_dir::test_seams::fail_next_rename_into(libc::ENOSYS);
    a.seams
        .crash
        .store(seams::Crash::AfterLink as u8, Ordering::SeqCst);
    assert!(a.write(room, "linked", &linked, start).is_err());
    assert_eq!(temps_in(a.dir()).len(), 2);
    assert_eq!(
        std::fs::read(a.dir().join(own_name("linked"))).unwrap(),
        linked
    );
    let a_run = a.run_id().to_string();
    drop(a);

    let b = open(root.path(), StoreConfig::default(), on(start, 1));
    let names = names_in(b.dir());
    let mut reports: Vec<String> = names.iter().map(|n| report_of(n)).collect();
    reports.sort();
    assert_eq!(reports, vec!["linked", "whole"], "{names:?}");
    let linked_name = names.iter().find(|n| report_of(n) == "linked").unwrap();
    assert_eq!(std::fs::read(b.dir().join(linked_name)).unwrap(), linked);
    assert_eq!(temps_in(b.dir()), Vec::<String>::new());
    assert!(
        every_file(root.path())
            .iter()
            .all(|(run, _)| run == b.run_id())
    );
    assert!(!runs_root(root.path()).join(lock_name(&a_run)).exists());
    assert_eq!(counts(&b).adopted_bytes, size_sum(b.dir()));
}
