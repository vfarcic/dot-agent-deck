//! Offline store contract: real files in tempdirs, synthetic owner identities,
//! and instance-local crash injection. See fixtures/release-check/store/README.md.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

// Recompile the production source under this test crate's cfg(test), so the
// fault seam is absent from the library and from every shipping binary.
#[rustfmt::skip]
#[allow(dead_code)]
#[path = "../src/release_check/store.rs"]
mod store;

use spec::spec;
use store::testing::{self, Checkpoint, CrashImage, Fault, FileKind, WriteStep};
use store::{Evidence, Journal, Key, Mark, Outcome, Owner, Publication, State, Store, Ticket};
use tempfile::TempDir;

const IMAGE: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

fn key(release: &str) -> Key {
    Key {
        platform: "linux".into(),
        agent: "claude".into(),
        release: release.into(),
        argv_class: "interactive".into(),
        executable_digest: "a".repeat(64),
        companion_digests: vec!["b".repeat(64)],
        deck_image_hash: IMAGE.into(),
    }
}

fn owner(start_time: u64) -> Owner {
    Owner {
        boot_id: "test-boot".into(),
        pid: 12345,
        start_time,
    }
}

fn mark(id: &str) -> Mark {
    Mark {
        owner: owner(100),
        ring_id: id.into(),
    }
}

fn evidence(outcome: Outcome) -> Evidence {
    Evidence {
        outcome,
        runner_image_hash: IMAGE.into(),
        body: b"bounded synthetic evidence".to_vec(),
    }
}

struct Fixture {
    temp: TempDir,
    store: Store,
}

fn scratch() -> TempDir {
    // The SC-03 assignment requires tempfile::tempdir() only: no shared
    // harness base, state dir, real HOME or process-global test environment.
    tempfile::tempdir().unwrap() // linkage-check:allow-bare-tempdir
}

impl Fixture {
    fn new() -> Self {
        let temp = scratch();
        let store = testing::open(&temp.path().join("records"), vec![owner(100)]).unwrap();
        Self { temp, store }
    }

    fn root(&self) -> std::path::PathBuf {
        self.temp.path().join("records")
    }

    fn reload(&mut self, image: CrashImage) {
        self.store = testing::recover(&self.root(), vec![owner(100)], image).unwrap();
        invariant(&self.store);
    }

    fn pass(&mut self, key: &Key) -> u64 {
        let ticket = self.store.queue(key).unwrap();
        published(
            self.store
                .publish(&ticket, evidence(Outcome::Passed), IMAGE)
                .unwrap(),
        )
    }

    fn trip(&mut self, key: &Key) -> u64 {
        let ring = mark("trip");
        self.store.ring_start(key, &ring).unwrap();
        self.store
            .ring_end(key, &ring, Some("echo_mismatch"))
            .unwrap();
        journal(&self.store, key).last_event
    }
}

fn published(result: Publication) -> u64 {
    match result {
        Publication::Published { epoch } => epoch,
        other => panic!("expected a published result, got {other:?}"),
    }
}

fn stale(store: &mut Store, ticket: &Ticket) {
    assert_eq!(
        store
            .publish(ticket, evidence(Outcome::Passed), IMAGE)
            .unwrap(),
        Publication::Stale
    );
}

fn journal(store: &Store, key: &Key) -> Journal {
    store
        .view(key)
        .unwrap()
        .journal
        .expect("journal must exist")
}

fn invariant(store: &Store) {
    let metadata = store.metadata().unwrap();
    assert!(
        metadata.epoch >= metadata.floor,
        "epoch below durable floor: {metadata:?}"
    );
    for key in store.keys().unwrap() {
        let view = store.view(&key).unwrap();
        let row = view.journal.unwrap();
        assert!(
            metadata.epoch >= row.last_event,
            "reused journal epoch: {metadata:?} {row:?}"
        );
        assert!(
            metadata.epoch >= row.revoked_through,
            "lost revocation epoch: {metadata:?} {row:?}"
        );
        if let Some(publication) = view.admission {
            assert_eq!(row.state, Some(State::Passed { publication }));
            assert!(publication > row.revoked_through);
            assert!(!view.pending_trip, "dead owner must prevent admission");
        }
    }
}

fn checkpoint(file: FileKind, step: WriteStep) -> Checkpoint {
    Checkpoint::Write { file, step }
}

fn write_points(file: FileKind) -> Vec<Checkpoint> {
    [
        WriteStep::TempSynced,
        WriteStep::Renamed,
        WriteStep::DirectorySynced,
    ]
    .into_iter()
    .map(|step| checkpoint(file, step))
    .collect()
}

fn crashed(result: Result<(), store::Error>, point: Checkpoint) {
    assert!(
        matches!(result, Err(store::Error::InjectedCrash { checkpoint }) if checkpoint == point),
        "fault did not stop at {point:?}: {result:?}"
    );
}

#[derive(Clone, Copy, Debug)]
enum Row {
    Queue,
    Publish,
    RingStart,
    RingEnd,
    Trip,
    Convert,
    Forget,
    Prune,
    Evict,
}

fn setup_row(row: Row) -> (Fixture, Key, Ticket) {
    let mut fixture = Fixture::new();
    let key = key("1.0.0");
    fixture.pass(&key);
    // Start with a nonzero watermark so "unchanged" also rejects an
    // implementation that accidentally resets it to zero on this row.
    fixture.trip(&key);
    fixture.pass(&key);
    if matches!(
        row,
        Row::Queue
            | Row::Publish
            | Row::RingStart
            | Row::RingEnd
            | Row::Trip
            | Row::Forget
            | Row::Evict
    ) {
        fixture.store.ring_start(&key, &mark("peer")).unwrap();
    }
    if matches!(row, Row::RingEnd | Row::Trip | Row::Convert) {
        fixture.store.ring_start(&key, &mark("row")).unwrap();
    }
    let ticket = fixture.store.queue(&key).unwrap();
    if matches!(row, Row::Convert) {
        testing::owners(&mut fixture.store, Vec::new());
    }
    (fixture, key, ticket)
}

fn apply_row(store: &mut Store, key: &Key, ticket: &Ticket, row: Row) -> Result<(), store::Error> {
    match row {
        Row::Queue => store.queue(key).map(|_| ()),
        Row::Publish => store
            .publish(ticket, evidence(Outcome::Passed), IMAGE)
            .map(|_| ()),
        Row::RingStart => store.ring_start(key, &mark("row")).map(|_| ()),
        Row::RingEnd => store.ring_end(key, &mark("row"), None),
        Row::Trip => store.ring_end(key, &mark("row"), Some("echo_mismatch")),
        Row::Convert => store.convert_dead_owners(key).map(|_| ()),
        Row::Forget => store.forget(key).map(|_| ()),
        Row::Prune => testing::prune_keys(store, std::slice::from_ref(key)),
        Row::Evict => store.evict_evidence(key),
    }
}

// Each row has both a clean transition check and a crash check at EVERY
// temp-file fsync, rename and directory-fsync of every file that it writes.
fn transition(row: Row) {
    let (mut fixture, key, ticket) = setup_row(row);
    let before = journal(&fixture.store, &key);
    let metadata = fixture.store.metadata().unwrap();
    apply_row(&mut fixture.store, &key, &ticket, row).unwrap();
    invariant(&fixture.store);
    let after = fixture.store.view(&key).unwrap();
    let new_metadata = fixture.store.metadata().unwrap();
    match row {
        Row::Prune => {
            assert!(after.journal.is_none());
            assert_eq!(new_metadata.epoch, metadata.epoch);
            assert_eq!(new_metadata.floor, metadata.epoch);
            stale(&mut fixture.store, &ticket);
        }
        Row::Evict => {
            assert_eq!(after.journal, Some(before.clone()));
            assert!(after.evidence.is_none());
            assert_eq!(new_metadata, metadata);
            published(
                fixture
                    .store
                    .publish(&ticket, evidence(Outcome::Passed), IMAGE)
                    .unwrap(),
            );
        }
        _ => {
            let current = after.journal.as_ref().unwrap();
            let epoch_event = matches!(
                row,
                Row::Queue | Row::Publish | Row::Trip | Row::Convert | Row::Forget
            );
            assert_eq!(new_metadata.epoch, metadata.epoch + u64::from(epoch_event));
            assert_eq!(new_metadata.floor, metadata.floor);
            match row {
                Row::Queue => {
                    let mut expected = before.clone();
                    expected.queued = Some(new_metadata.epoch);
                    assert_eq!(*current, expected);
                    published(
                        fixture
                            .store
                            .publish(&ticket, evidence(Outcome::Passed), IMAGE)
                            .unwrap(),
                    );
                }
                Row::Publish => {
                    let mut expected = before.clone();
                    expected.last_event = new_metadata.epoch;
                    expected.state = Some(State::Passed {
                        publication: new_metadata.epoch,
                    });
                    expected.queued = None;
                    assert_eq!(*current, expected);
                    stale(&mut fixture.store, &ticket);
                }
                Row::RingStart | Row::RingEnd => {
                    let mut expected = before.clone();
                    if matches!(row, Row::RingStart) {
                        expected.ringing.push(mark("row"));
                    } else {
                        expected.ringing.retain(|m| m != &mark("row"));
                    }
                    assert_eq!(*current, expected);
                    published(
                        fixture
                            .store
                            .publish(&ticket, evidence(Outcome::Passed), IMAGE)
                            .unwrap(),
                    );
                }
                Row::Trip | Row::Convert | Row::Forget => {
                    let mut expected = before.clone();
                    expected.last_event = new_metadata.epoch;
                    expected.revoked_through = new_metadata.epoch;
                    expected.state = Some(if matches!(row, Row::Forget) {
                        State::Forgotten {
                            at: new_metadata.epoch,
                        }
                    } else {
                        State::Revoked {
                            through: new_metadata.epoch,
                            cause: if matches!(row, Row::Convert) {
                                "ring owner stopped".into()
                            } else {
                                "echo_mismatch".into()
                            },
                        }
                    });
                    if !matches!(row, Row::Forget) {
                        expected.ringing.retain(|m| m != &mark("row"));
                    }
                    assert_eq!(*current, expected);
                    if matches!(row, Row::Forget) {
                        assert!(after.evidence.is_none());
                    }
                    stale(&mut fixture.store, &ticket);
                }
                _ => unreachable!(),
            }
        }
    }

    let points = match row {
        Row::Prune => vec![
            Checkpoint::PruneBeforeFloor,
            Checkpoint::PruneFloorDurable,
            Checkpoint::PruneAfterUnlink { index: 0 },
            Checkpoint::PruneUnlinkDurable { index: 0 },
        ],
        Row::Evict => vec![
            Checkpoint::EvidenceUnlinked,
            Checkpoint::EvidenceUnlinkDurable,
        ],
        Row::RingStart | Row::RingEnd => write_points(FileKind::Journal),
        Row::Queue | Row::Publish | Row::Trip | Row::Convert | Row::Forget => {
            let mut points = write_points(FileKind::Journal);
            points.extend(write_points(FileKind::Store));
            if matches!(row, Row::Publish) {
                points.extend(write_points(FileKind::Evidence));
            }
            if matches!(row, Row::Forget) {
                points.extend([
                    Checkpoint::EvidenceUnlinked,
                    Checkpoint::EvidenceUnlinkDurable,
                ]);
            }
            points
        }
    };
    for point in points {
        for image in [CrashImage::KeepUnsynced, CrashImage::DropUnsynced] {
            let (mut fixture, key, ticket) = setup_row(row);
            let old = journal(&fixture.store, &key);
            testing::arm(&mut fixture.store, Fault::CrashAt(point));
            crashed(apply_row(&mut fixture.store, &key, &ticket, row), point);
            fixture.reload(image);
            let recovered = fixture.store.view(&key).unwrap();
            if let Some(current) = &recovered.journal {
                assert!(
                    Some(current) == after.journal.as_ref() || current == &old,
                    "torn journal after {row:?} at {point:?}, image {image:?}: {current:?}"
                );
                assert!(current.revoked_through >= old.revoked_through);
            } else {
                assert!(matches!(row, Row::Prune), "lost journal: {row:?} {point:?}");
                assert!(fixture.store.metadata().unwrap().floor >= ticket.queued);
            }
            let observed = fixture.store.metadata().unwrap().epoch;
            let next = fixture.store.queue(&key).unwrap();
            assert!(
                next.queued > observed,
                "reused epoch after {row:?} at {point:?}"
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum PrunePoint {
    BeforeFloor,
    FloorDurable,
    UnlinkUnsynced,
    UnlinkDurable,
    Recreated,
}

#[derive(Clone, Copy, Debug)]
enum PruneColumn {
    ChosenWithEvent,
    ChosenNoEvent,
    OtherNoEvent,
}

fn pruning(point: PrunePoint, column: PruneColumn) {
    for image in [CrashImage::KeepUnsynced, CrashImage::DropUnsynced] {
        let mut fixture = Fixture::new();
        let key = key("1.0.0");
        let other = key_with_release("2.0.0");
        let ticket = fixture.store.queue(&key).unwrap();
        fixture.store.queue(&other).unwrap();
        if matches!(column, PruneColumn::ChosenWithEvent) {
            fixture.store.forget(&key).unwrap();
        }
        let chosen = if matches!(column, PruneColumn::OtherNoEvent) {
            &other
        } else {
            &key
        };
        let checkpoint = match point {
            PrunePoint::BeforeFloor => Checkpoint::PruneBeforeFloor,
            PrunePoint::FloorDurable => Checkpoint::PruneFloorDurable,
            PrunePoint::UnlinkUnsynced => Checkpoint::PruneAfterUnlink { index: 0 },
            PrunePoint::UnlinkDurable | PrunePoint::Recreated => {
                Checkpoint::PruneUnlinkDurable { index: 0 }
            }
        };
        testing::arm(&mut fixture.store, Fault::CrashAt(checkpoint));
        crashed(
            testing::prune_keys(&mut fixture.store, std::slice::from_ref(chosen)),
            checkpoint,
        );
        fixture.reload(image);
        // Assert against the RECOVERED directory, and force both outcomes of
        // the unsynced unlink rather than trusting the host filesystem.
        let absent = fixture.store.view(&key).unwrap().journal.is_none();
        if matches!(point, PrunePoint::UnlinkUnsynced)
            && !matches!(column, PruneColumn::OtherNoEvent)
        {
            assert_eq!(absent, matches!(image, CrashImage::KeepUnsynced));
        }
        if matches!(point, PrunePoint::Recreated) {
            fixture.store.queue(chosen).unwrap();
            let recreated = journal(&fixture.store, chosen);
            let floor = fixture.store.metadata().unwrap().floor;
            assert_eq!(recreated.last_event, floor);
            assert_eq!(recreated.revoked_through, floor);
            fixture.reload(CrashImage::KeepUnsynced);
        }
        let rejects = matches!(column, PruneColumn::ChosenWithEvent)
            || (!matches!(column, PruneColumn::OtherNoEvent)
                && (absent || matches!(point, PrunePoint::Recreated)));
        let recovered = snapshot(&fixture.store, &key);
        fixture.reload(CrashImage::KeepUnsynced);
        assert_eq!(
            snapshot(&fixture.store, &key),
            recovered,
            "clean reload changed checkpoint decision: {point:?} {column:?} {image:?}"
        );
        let result = fixture
            .store
            .publish(&ticket, evidence(Outcome::Passed), IMAGE)
            .unwrap();
        if rejects {
            assert_eq!(result, Publication::Stale, "{point:?} {column:?} {image:?}");
        } else {
            published(result);
        }
        invariant(&fixture.store);
        fixture.reload(CrashImage::KeepUnsynced);
    }
}

fn key_with_release(release: &str) -> Key {
    key(release)
}

fn snapshot(store: &Store, key: &Key) -> (store::Metadata, Option<Journal>) {
    (store.metadata().unwrap(), store.view(key).unwrap().journal)
}

fn recreation(trip: bool) {
    for reload_each_step in [false, true] {
        for point in [
            None,
            Some(Checkpoint::CreateJournalBeforeWrite),
            Some(Checkpoint::CreateJournalDurable),
        ] {
            for image in [CrashImage::KeepUnsynced, CrashImage::DropUnsynced] {
                let mut f = Fixture::new();
                let key = key("1.0.0");
                f.pass(&key);
                let old = f.store.queue(&key).unwrap();
                if reload_each_step {
                    f.reload(CrashImage::KeepUnsynced);
                }
                if trip {
                    f.trip(&key);
                } else {
                    f.store.forget(&key).unwrap();
                }
                if reload_each_step {
                    f.reload(CrashImage::KeepUnsynced);
                }
                testing::prune_keys(&mut f.store, std::slice::from_ref(&key)).unwrap();
                if reload_each_step {
                    f.reload(CrashImage::KeepUnsynced);
                }
                let floor = f.store.metadata().unwrap().floor;
                assert!(floor >= old.queued);
                let newer = if let Some(point) = point {
                    testing::arm(&mut f.store, Fault::CrashAt(point));
                    crashed(f.store.queue(&key).map(|_| ()), point);
                    f.reload(image);
                    if let Some(row) = f.store.view(&key).unwrap().journal {
                        assert_eq!(row.last_event, floor);
                        assert_eq!(row.revoked_through, floor);
                        assert_eq!(row.state, None);
                    }
                    f.store.queue(&key).unwrap()
                } else {
                    f.store.queue(&key).unwrap()
                };
                let row = journal(&f.store, &key);
                assert_eq!(row.last_event, floor);
                assert_eq!(row.revoked_through, floor);
                assert_eq!(row.state, None);
                assert!(row.ringing.is_empty());
                if reload_each_step {
                    f.reload(CrashImage::KeepUnsynced);
                }
                stale(&mut f.store, &old);
                if reload_each_step {
                    f.reload(CrashImage::KeepUnsynced);
                }
                let publication = published(
                    f.store
                        .publish(&newer, evidence(Outcome::Passed), IMAGE)
                        .unwrap(),
                );
                assert_eq!(f.store.view(&key).unwrap().admission, Some(publication));
                assert_eq!(journal(&f.store, &key).revoked_through, floor);
            }
        }
    }
}

fn conversion_controls() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    let live = mark("live");
    let old_start = Mark {
        owner: owner(99),
        ring_id: "reused-pid".into(),
    };
    let old_boot = Mark {
        owner: Owner {
            boot_id: "previous-boot".into(),
            ..owner(100)
        },
        ring_id: "old-boot".into(),
    };
    let missing_pid = Mark {
        owner: Owner {
            pid: 54321,
            ..owner(100)
        },
        ring_id: "missing-pid".into(),
    };
    testing::owners(
        &mut f.store,
        vec![
            live.owner.clone(),
            old_start.owner.clone(),
            old_boot.owner.clone(),
            missing_pid.owner.clone(),
        ],
    );
    for ring in [&live, &old_start, &old_boot, &missing_pid] {
        f.store.ring_start(&key, ring).unwrap();
    }
    testing::owners(&mut f.store, vec![owner(100)]);
    assert!(f.store.view(&key).unwrap().pending_trip);
    let queued_before = f.store.queue(&key).unwrap();
    let epoch = f
        .store
        .convert_dead_owners(&key)
        .unwrap()
        .expect("a conversion happened");
    assert_eq!(journal(&f.store, &key).ringing, vec![live.clone()]);
    assert_eq!(journal(&f.store, &key).revoked_through, epoch);
    assert!(!f.store.view(&key).unwrap().pending_trip);
    stale(&mut f.store, &queued_before);
    let before_noop = f.store.metadata().unwrap();
    assert_eq!(f.store.convert_dead_owners(&key).unwrap(), None);
    assert_eq!(f.store.metadata().unwrap(), before_noop);
    f.store.ring_end(&key, &live, None).unwrap();
    assert!(journal(&f.store, &key).ringing.is_empty());
    let publication = f.pass(&key);
    assert!(publication > epoch);
    assert_eq!(f.store.view(&key).unwrap().admission, Some(publication));
}

fn symlink_controls() {
    let temp = scratch();
    let target = temp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(target.join("sentinel"), b"do not touch").unwrap();
    let root = temp.path().join("records");
    symlink(&target, &root).unwrap();
    assert!(testing::open(&root, vec![owner(100)]).is_err());
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"do not touch");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 1);

    for component in [
        FileKind::Store,
        FileKind::Journal,
        FileKind::Evidence,
        FileKind::Lock,
    ] {
        let mut f = Fixture::new();
        let key = key("1.0.0");
        f.pass(&key);
        let sentinel = f.temp.path().join("outside-sentinel");
        let path = testing::path(&f.store, &key, component);
        // Keep the target VALID for this component: following it must not
        // masquerade as no-follow refusal through a JSON parse failure.
        let original = fs::read(&path).unwrap();
        fs::write(&sentinel, &original).unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&sentinel, &path).unwrap();
        let refused = match component {
            FileKind::Journal | FileKind::Evidence => f.store.view(&key).is_err(),
            _ => f.store.queue(&key).is_err(),
        };
        assert!(refused, "followed symlinked {component:?}");
        assert_eq!(fs::read(&sentinel).unwrap(), original);
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }
}

fn validation_controls() {
    let mut f = Fixture::new();
    for release in ["a".to_owned(), "a".repeat(64), "1.2.3+build-alpha".into()] {
        f.store.queue(&key(&release)).unwrap();
    }
    let mut invalid = Vec::new();
    for release in [
        "".to_owned(),
        "a".repeat(65),
        "../escape".into(),
        "/absolute".into(),
        "a/b".into(),
        "a\\b".into(),
        "é".into(),
        "a\nb".into(),
        "a\0b".into(),
    ] {
        invalid.push(key(&release));
    }
    for agent in ["not-an-agent", "../claude", "claude/other"] {
        invalid.push(Key {
            agent: agent.into(),
            ..key("1.0.0")
        });
    }
    for digest in [
        "a".repeat(63),
        "a".repeat(65),
        "z".repeat(64),
        "../escape".into(),
    ] {
        invalid.push(Key {
            executable_digest: digest.clone(),
            ..key("1.0.0")
        });
        invalid.push(Key {
            deck_image_hash: digest.clone(),
            ..key("1.0.0")
        });
        invalid.push(Key {
            companion_digests: vec![digest],
            ..key("1.0.0")
        });
    }
    let before = f.store.metadata().unwrap();
    let before_keys = f.store.keys().unwrap();
    for invalid in invalid {
        assert!(
            f.store.queue(&invalid).is_err(),
            "accepted invalid filename input: {invalid:?}"
        );
        assert!(
            f.store.view(&invalid).is_err(),
            "read accepted invalid filename input: {invalid:?}"
        );
        assert!(
            f.store.forget(&invalid).is_err(),
            "forget accepted invalid filename input: {invalid:?}"
        );
        assert!(
            f.store.evict_evidence(&invalid).is_err(),
            "unlink accepted invalid filename input: {invalid:?}"
        );
    }
    assert_eq!(f.store.metadata().unwrap(), before);
    assert_eq!(f.store.keys().unwrap(), before_keys);
    assert_eq!(
        fs::metadata(f.root()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for entry in fs::read_dir(f.root()).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

fn random_next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn random_rows(seed: u64) {
    eprintln!("watermark property replay seed={seed:#018x}");
    let mut rng = seed;
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    let mut watermark = 0;
    let crash_index = random_next(&mut rng) as usize % 64;
    let mut crash_seen = false;
    for index in 0..64 {
        let action = random_next(&mut rng) % 9;
        if index == crash_index {
            // Queue is always reachable. Its random crash boundary makes the
            // crash nonvacuous even when other generated rows would be no-ops.
            let points = [
                write_points(FileKind::Journal),
                write_points(FileKind::Store),
            ]
            .concat();
            let point = points[random_next(&mut rng) as usize % points.len()];
            testing::arm(&mut f.store, Fault::CrashAt(point));
            crashed(f.store.queue(&key).map(|_| ()), point);
            crash_seen = true;
            let image = if random_next(&mut rng) & 1 == 0 {
                CrashImage::KeepUnsynced
            } else {
                CrashImage::DropUnsynced
            };
            f.reload(image);
        } else {
            match action {
                0 => {
                    f.store.queue(&key).unwrap();
                }
                1 => {
                    let ticket = f.store.queue(&key).unwrap();
                    let outcome = match random_next(&mut rng) % 4 {
                        0 => Outcome::Passed,
                        1 => Outcome::Inconclusive {
                            reason: "fixture".into(),
                        },
                        2 => Outcome::Differs {
                            fact: "echo".into(),
                        },
                        _ => Outcome::Unavailable {
                            reason: "fixture".into(),
                        },
                    };
                    published(f.store.publish(&ticket, evidence(outcome), IMAGE).unwrap());
                }
                2 => {
                    if f.store.view(&key).unwrap().admission.is_none() {
                        f.pass(&key);
                    }
                    f.store
                        .ring_start(&key, &mark(&format!("random-{index}")))
                        .unwrap();
                }
                3 | 4 => {
                    if let Some(row) = f.store.view(&key).unwrap().journal {
                        if let Some(mark) = row.ringing.first() {
                            f.store
                                .ring_end(&key, mark, (action == 4).then_some("echo_mismatch"))
                                .unwrap();
                        }
                    }
                }
                5 => {
                    if f.store.view(&key).unwrap().admission.is_none() {
                        f.pass(&key);
                    }
                    f.store
                        .ring_start(&key, &mark(&format!("convert-{index}")))
                        .unwrap();
                    testing::owners(&mut f.store, Vec::new());
                    f.store.convert_dead_owners(&key).unwrap();
                    testing::owners(&mut f.store, vec![owner(100)]);
                }
                6 => {
                    f.store.forget(&key).unwrap();
                }
                7 => {
                    testing::prune_keys(&mut f.store, std::slice::from_ref(&key)).unwrap();
                }
                8 => {
                    f.store.evict_evidence(&key).unwrap();
                }
                _ => unreachable!(),
            }
            invariant(&f.store);
            f.reload(CrashImage::KeepUnsynced);
        }
        let current = f.store.view(&key).unwrap().journal.map_or_else(
            || f.store.metadata().unwrap().floor,
            |row| row.revoked_through,
        );
        assert!(
            current >= watermark,
            "watermark decreased seed={seed:#018x}, step={index}, row={action}, old={watermark}, new={current}"
        );
        watermark = current;
    }
    assert!(crash_seen, "random crash not exercised, seed={seed:#018x}");
}

/// Scenario: Queue another check for a measured release; it advances only the store epoch and queued detail. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_queue() {
    transition(Row::Queue);
}

/// Scenario: Publish a passing check; its fresh publication fences old completions and preserves all ring marks. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_publish() {
    transition(Row::Publish);
}

/// Scenario: Begin a ring on an admitted release; the durable mark changes neither authority nor a queued check's eligibility. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_ring_start() {
    transition(Row::RingStart);
}

/// Scenario: End a ring without a trip; only its own mark disappears and the queued check can still publish. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_ring_end() {
    transition(Row::RingEnd);
}

/// Scenario: End a ring with an echo mismatch; the new revocation fences queued checks and removes that ring's mark. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_trip() {
    transition(Row::Trip);
}

/// Scenario: Recover a stopped ring owner; the conversion revokes its publication and fences queued checks. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_conversion() {
    transition(Row::Convert);
}

/// Scenario: Forget a measured release; evidence is removed, its durable journal revokes old checks, and existing marks remain. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_forget() {
    transition(Row::Forget);
}

/// Scenario: Prune an unmarked journal; the floor is durable before removal and prevents old completions from publishing. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_prune() {
    transition(Row::Prune);
}

/// Scenario: Evict a measured release's evidence; its journal and queued check survive unchanged. Crash each write boundary and reopen the store to check atomic journals and nonreused epochs.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_row_evidence_eviction() {
    transition(Row::Evict);
}

/// Scenario: Queue a check in the chosen with event pruning case and crash at before floor. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_before_floor_chosen_with_event() {
    pruning(PrunePoint::BeforeFloor, PruneColumn::ChosenWithEvent);
}

/// Scenario: Queue a check in the chosen no event pruning case and crash at before floor. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_before_floor_chosen_no_event() {
    pruning(PrunePoint::BeforeFloor, PruneColumn::ChosenNoEvent);
}

/// Scenario: Queue a check in the other no event pruning case and crash at before floor. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_before_floor_other_no_event() {
    pruning(PrunePoint::BeforeFloor, PruneColumn::OtherNoEvent);
}

/// Scenario: Queue a check in the chosen with event pruning case and crash at floor durable. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_floor_durable_chosen_with_event() {
    pruning(PrunePoint::FloorDurable, PruneColumn::ChosenWithEvent);
}

/// Scenario: Queue a check in the chosen no event pruning case and crash at floor durable. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_floor_durable_chosen_no_event() {
    pruning(PrunePoint::FloorDurable, PruneColumn::ChosenNoEvent);
}

/// Scenario: Queue a check in the other no event pruning case and crash at floor durable. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_floor_durable_other_no_event() {
    pruning(PrunePoint::FloorDurable, PruneColumn::OtherNoEvent);
}

/// Scenario: Queue a check in the chosen with event pruning case and crash at unlink unsynced. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_unlink_unsynced_chosen_with_event() {
    pruning(PrunePoint::UnlinkUnsynced, PruneColumn::ChosenWithEvent);
}

/// Scenario: Queue a check in the chosen no event pruning case and crash at unlink unsynced. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_unlink_unsynced_chosen_no_event() {
    pruning(PrunePoint::UnlinkUnsynced, PruneColumn::ChosenNoEvent);
}

/// Scenario: Queue a check in the other no event pruning case and crash at unlink unsynced. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_unlink_unsynced_other_no_event() {
    pruning(PrunePoint::UnlinkUnsynced, PruneColumn::OtherNoEvent);
}

/// Scenario: Queue a check in the chosen with event pruning case and crash at unlink durable. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_unlink_durable_chosen_with_event() {
    pruning(PrunePoint::UnlinkDurable, PruneColumn::ChosenWithEvent);
}

/// Scenario: Queue a check in the chosen no event pruning case and crash at unlink durable. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_unlink_durable_chosen_no_event() {
    pruning(PrunePoint::UnlinkDurable, PruneColumn::ChosenNoEvent);
}

/// Scenario: Queue a check in the other no event pruning case and crash at unlink durable. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_unlink_durable_other_no_event() {
    pruning(PrunePoint::UnlinkDurable, PruneColumn::OtherNoEvent);
}

/// Scenario: Queue a check in the chosen with event pruning case and crash at recreated. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_recreated_chosen_with_event() {
    pruning(PrunePoint::Recreated, PruneColumn::ChosenWithEvent);
}

/// Scenario: Queue a check in the chosen no event pruning case and crash at recreated. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_recreated_chosen_no_event() {
    pruning(PrunePoint::Recreated, PruneColumn::ChosenNoEvent);
}

/// Scenario: Queue a check in the other no event pruning case and crash at recreated. Recover both possible unsynced directory images, check the checkpoint table's publication decision, and reload cleanly.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_recreated_other_no_event() {
    pruning(PrunePoint::Recreated, PruneColumn::OtherNoEvent);
}

/// Scenario: Queue a re-check, then trip the admitted ring before the check completes. The old pass must be discarded after a clean reload.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_stale_pass_loses_to_trip() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    let queued = f.store.queue(&key).unwrap();
    let trip = f.trip(&key);
    f.reload(CrashImage::KeepUnsynced);
    stale(&mut f.store, &queued);
    assert_eq!(journal(&f.store, &key).revoked_through, trip);
    assert!(f.store.view(&key).unwrap().admission.is_none());
}

/// Scenario: Queue a re-check, trip the release and Forget it. A late pass cannot resurrect the forgotten release or its removed evidence.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_trip_forget_stale_pass() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    let queued = f.store.queue(&key).unwrap();
    f.trip(&key);
    let at = f.store.forget(&key).unwrap();
    f.reload(CrashImage::KeepUnsynced);
    stale(&mut f.store, &queued);
    let view = f.store.view(&key).unwrap();
    assert_eq!(view.journal.unwrap().state, Some(State::Forgotten { at }));
    assert!(view.evidence.is_none());
}

/// Scenario: Queue a re-check, trip its release and evict the evidence. A late passing completion is still rejected by the retained revocation journal.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_trip_evict_stale_pass() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    let queued = f.store.queue(&key).unwrap();
    let trip = f.trip(&key);
    f.store.evict_evidence(&key).unwrap();
    f.reload(CrashImage::KeepUnsynced);
    stale(&mut f.store, &queued);
    assert_eq!(journal(&f.store, &key).revoked_through, trip);
}

/// Scenario: Queue a check and durably prune its journal. The floor rejects the late completion and the store still has no admission.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_pruned_stale_pass() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    let queued = f.store.queue(&key).unwrap();
    testing::prune_keys(&mut f.store, std::slice::from_ref(&key)).unwrap();
    f.reload(CrashImage::KeepUnsynced);
    assert!(f.store.view(&key).unwrap().journal.is_none());
    stale(&mut f.store, &queued);
    assert!(f.store.view(&key).unwrap().admission.is_none());
}

/// Scenario: Queue a check, delete only its tempdir store and create a fresh one at the same path. The changed store id rejects the old ticket even if its epoch would otherwise pass.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_recreated_store_rejects_old_ticket() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    let queued = f.store.queue(&key).unwrap();
    let old_id = f.store.metadata().unwrap().store_id;
    drop(f.store);
    fs::remove_dir_all(f.temp.path().join("records")).unwrap();
    f.store = testing::open(&f.temp.path().join("records"), vec![owner(100)]).unwrap();
    assert_ne!(f.store.metadata().unwrap().store_id, old_id);
    stale(&mut f.store, &queued);
    assert!(f.store.view(&key).unwrap().admission.is_none());
}

/// Scenario: Trip a measured release, then queue a new check. Its pass admits at a new publication while keeping the trip's durable watermark.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_fresh_recheck_after_trip_admits() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    let trip = f.trip(&key);
    let publication = f.pass(&key);
    assert!(publication > trip);
    let view = f.store.view(&key).unwrap();
    assert_eq!(view.admission, Some(publication));
    assert_eq!(view.journal.unwrap().revoked_through, trip);
}

/// Scenario: Crash a publication after the journal directory is synced but before the store metadata is written. Reload must recover the publication epoch and allocate strictly above it.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_journal_ahead_of_store_reuses_no_epoch() {
    for image in [CrashImage::KeepUnsynced, CrashImage::DropUnsynced] {
        let mut f = Fixture::new();
        let key = key("1.0.0");
        let queued = f.store.queue(&key).unwrap();
        let point = checkpoint(FileKind::Journal, WriteStep::DirectorySynced);
        testing::arm(&mut f.store, Fault::CrashAt(point));
        crashed(
            f.store
                .publish(&queued, evidence(Outcome::Passed), IMAGE)
                .map(|_| ()),
            point,
        );
        f.reload(image);
        let last = journal(&f.store, &key).last_event;
        assert!(last > queued.queued);
        assert!(f.store.queue(&key).unwrap().queued > last);
    }
}

/// Scenario: Start a durable ring and simulate its owner disappearing. Reloaded Details must show PendingTrip and refuse admission before conversion.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_dead_owner_pending_trip() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    f.store.ring_start(&key, &mark("stopped")).unwrap();
    f.reload(CrashImage::KeepUnsynced);
    testing::owners(&mut f.store, Vec::new());
    let view = f.store.view(&key).unwrap();
    assert!(view.pending_trip);
    assert!(view.admission.is_none());
    assert_eq!(view.journal.unwrap().ringing, vec![mark("stopped")]);
}

/// Scenario: Refuse each persistence step while starting a local-record ring. No permit or first byte is produced, and recovery is either the old state or PendingTrip for an orphaned durable mark.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_refused_mark_write_prevents_first_byte() {
    for point in write_points(FileKind::Journal) {
        let mut f = Fixture::new();
        let key = key("1.0.0");
        f.pass(&key);
        testing::arm(&mut f.store, Fault::RefuseAt(point));
        let mut bytes = Vec::new();
        let result = f.store.ring_start(&key, &mark("no-byte"));
        if result.is_ok() {
            bytes.extend_from_slice(b"first byte");
        }
        assert!(
            result.is_err(),
            "refused write unexpectedly granted a permit at {point:?}"
        );
        assert!(
            bytes.is_empty(),
            "a ring wrote without a durable mark at {point:?}"
        );
        f.reload(CrashImage::KeepUnsynced);
        testing::owners(&mut f.store, Vec::new());
        let view = f.store.view(&key).unwrap();
        if !view.journal.unwrap().ringing.is_empty() {
            assert!(view.pending_trip);
            assert!(view.admission.is_none());
        }
    }
}

/// Scenario: Replace the records directory, metadata, journal, evidence and lock in separate tempdir cases with links to sentinel files. Opening or modifying the store must refuse each link and leave its target untouched.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_symlinked_components_refused() {
    symlink_controls();
}

/// Scenario: Open the store, rename its records directory, and replace its old pathname with a symlink to a sentinel directory. Evidence eviction, Forget and pruning must operate through the already-open directory descriptor and leave the sentinel untouched.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_removal_stays_anchored_after_directory_rename() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    let evidence_name = testing::path(&f.store, &key, FileKind::Evidence)
        .file_name()
        .unwrap()
        .to_owned();
    let journal_name = testing::path(&f.store, &key, FileKind::Journal)
        .file_name()
        .unwrap()
        .to_owned();
    let anchored = f.temp.path().join("anchored");
    let sentinel = f.temp.path().join("sentinel");
    fs::rename(f.root(), &anchored).unwrap();
    fs::create_dir(&sentinel).unwrap();
    fs::write(sentinel.join(&evidence_name), b"keep evidence").unwrap();
    fs::write(sentinel.join(&journal_name), b"keep journal").unwrap();
    symlink(&sentinel, f.root()).unwrap();
    f.store.evict_evidence(&key).unwrap();
    assert!(!anchored.join(&evidence_name).exists());
    assert!(anchored.join(&journal_name).exists());
    f.pass(&key);
    f.store.forget(&key).unwrap();
    assert!(!anchored.join(&evidence_name).exists());
    testing::prune_keys(&mut f.store, std::slice::from_ref(&key)).unwrap();
    assert!(!anchored.join(&journal_name).exists());
    assert_eq!(
        fs::read(sentinel.join(&evidence_name)).unwrap(),
        b"keep evidence"
    );
    assert_eq!(
        fs::read(sentinel.join(&journal_name)).unwrap(),
        b"keep journal"
    );
    assert_eq!(fs::read_dir(sentinel).unwrap().count(), 2);
}

/// Scenario: Try to ring a queued-only key and a key with each nonpassing result, then after Forget and a durable trip. Every refusal leaves the journal and epoch unchanged and grants no first-write permit.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_nonpassing_states_refuse_ring_without_mutation() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.store.queue(&key).unwrap();
    for outcome in [
        None,
        Some(Outcome::Differs {
            fact: "echo".into(),
        }),
        Some(Outcome::Inconclusive {
            reason: "fixture".into(),
        }),
        Some(Outcome::Unavailable {
            reason: "fixture".into(),
        }),
    ] {
        if let Some(outcome) = outcome {
            let ticket = f.store.queue(&key).unwrap();
            published(f.store.publish(&ticket, evidence(outcome), IMAGE).unwrap());
        }
        let before = snapshot(&f.store, &key);
        assert!(f.store.ring_start(&key, &mark("refused")).is_err());
        assert_eq!(snapshot(&f.store, &key), before);
    }
    f.store.forget(&key).unwrap();
    let before = snapshot(&f.store, &key);
    assert!(f.store.ring_start(&key, &mark("forgotten")).is_err());
    assert_eq!(snapshot(&f.store, &key), before);
    f.pass(&key);
    f.trip(&key);
    let before = snapshot(&f.store, &key);
    assert!(f.store.ring_start(&key, &mark("revoked")).is_err());
    assert_eq!(snapshot(&f.store, &key), before);
}

/// Scenario: Queue a check on a new key without intervening authority events. The queue leaves last_event at the floor, so its own passing result must publish.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_uncontended_queue_publishes() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    let floor = f.store.metadata().unwrap().floor;
    let queued = f.store.queue(&key).unwrap();
    let row = journal(&f.store, &key);
    assert_eq!(row.last_event, floor);
    assert_eq!(row.revoked_through, floor);
    assert_eq!(row.state, None);
    assert!(row.ringing.is_empty());
    assert!(queued.queued > row.last_event);
    assert_eq!(f.store.metadata().unwrap().epoch, queued.queued);
    published(
        f.store
            .publish(&queued, evidence(Outcome::Passed), IMAGE)
            .unwrap(),
    );
}

/// Scenario: Crash after the queue epoch is durable and before a runner could launch. Reload must allocate above that durable q even though queue never returned its ticket.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_queue_crash_before_launch() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    let point = Checkpoint::QueuePersistedBeforeLaunch;
    testing::arm(&mut f.store, Fault::CrashAt(point));
    crashed(f.store.queue(&key).map(|_| ()), point);
    f.reload(CrashImage::DropUnsynced);
    let persisted_q = f.store.metadata().unwrap().epoch;
    assert!(persisted_q > 0);
    assert!(f.store.queue(&key).unwrap().queued > persisted_q);
}

/// Scenario: Queue three keys and crash before the floor, after the floor, and after each unsynced or durable unlink. Each reload keeps G at least the floor, rejects completions for absent keys, and preserves untouched keys.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_between_unlinks_keeps_floor() {
    for point in [
        Checkpoint::PruneBeforeFloor,
        Checkpoint::PruneFloorDurable,
        Checkpoint::PruneAfterUnlink { index: 0 },
        Checkpoint::PruneUnlinkDurable { index: 0 },
        Checkpoint::PruneAfterUnlink { index: 1 },
        Checkpoint::PruneUnlinkDurable { index: 1 },
    ] {
        for image in [CrashImage::KeepUnsynced, CrashImage::DropUnsynced] {
            let mut f = Fixture::new();
            let keys = [key("1.0.0"), key("2.0.0"), key("3.0.0")];
            let tickets: Vec<_> = keys.iter().map(|key| f.store.queue(key).unwrap()).collect();
            testing::arm(&mut f.store, Fault::CrashAt(point));
            crashed(testing::prune_keys(&mut f.store, &keys[..2]), point);
            f.reload(image);
            let meta = f.store.metadata().unwrap();
            assert!(meta.epoch >= meta.floor);
            for (key, ticket) in keys.iter().zip(tickets) {
                if f.store.view(key).unwrap().journal.is_none() {
                    assert!(meta.floor >= ticket.queued);
                    stale(&mut f.store, &ticket);
                } else {
                    published(
                        f.store
                            .publish(&ticket, evidence(Outcome::Passed), IMAGE)
                            .unwrap(),
                    );
                }
            }
        }
    }
}

/// Scenario: Start live and stopped-owner rings, then request pruning of every journal. Both marked journals survive; only the unmarked key is removed.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_marked_journal_never_pruned() {
    let mut f = Fixture::new();
    let keys = [key("1.0.0"), key("2.0.0"), key("3.0.0")];
    for key in &keys {
        f.pass(key);
    }
    f.store.ring_start(&keys[0], &mark("live")).unwrap();
    let dead = Mark {
        owner: owner(99),
        ring_id: "dead".into(),
    };
    testing::owners(&mut f.store, vec![owner(100), owner(99)]);
    f.store.ring_start(&keys[1], &dead).unwrap();
    testing::owners(&mut f.store, vec![owner(100)]);
    testing::prune_keys(&mut f.store, &keys).unwrap();
    assert_eq!(journal(&f.store, &keys[0]).ringing, vec![mark("live")]);
    assert_eq!(journal(&f.store, &keys[1]).ringing, vec![dead]);
    assert!(f.store.view(&keys[2]).unwrap().journal.is_none());
}

/// Scenario: Recover marks with the same pid but different start times, another boot id and a missing pid. Conversion keeps the genuinely live ring, removes only dead marks, and a later clean end and passing re-check admit with no marks left.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_conversion_only_dead_marks_then_recheck() {
    conversion_controls();
}

/// Scenario: Observe a ring owner as dead, then make the same owner live before conversion acquires its authority. Conversion must recheck its identity, keep its mark and allocate no revocation epoch.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_conversion_rechecks_owner_before_commit() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    f.store.ring_start(&key, &mark("returns")).unwrap();
    testing::owners(&mut f.store, Vec::new());
    assert!(f.store.view(&key).unwrap().pending_trip);
    testing::owners(&mut f.store, vec![owner(100)]);
    let before = snapshot(&f.store, &key);
    assert_eq!(f.store.convert_dead_owners(&key).unwrap(), None);
    assert_eq!(snapshot(&f.store, &key), before);
    assert_eq!(journal(&f.store, &key).ringing, vec![mark("returns")]);
}

/// Scenario: Refuse each conversion journal write with a dead ring owner. Details stays PendingTrip until a subsequent observation retries conversion, then a fresh passing re-check admits.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_conversion_write_failure_retries() {
    for point in write_points(FileKind::Journal) {
        let mut f = Fixture::new();
        let key = key("1.0.0");
        f.pass(&key);
        f.store.ring_start(&key, &mark("dead")).unwrap();
        testing::owners(&mut f.store, Vec::new());
        testing::arm(&mut f.store, Fault::RefuseAt(point));
        assert!(f.store.convert_dead_owners(&key).is_err());
        assert!(
            f.store.view(&key).unwrap().pending_trip,
            "failed conversion lost PendingTrip at {point:?}"
        );
        assert!(f.store.view(&key).unwrap().admission.is_none());
        f.store.observe(&key).unwrap();
        assert!(!f.store.view(&key).unwrap().pending_trip);
        assert!(journal(&f.store, &key).ringing.is_empty());
        let publication = f.pass(&key);
        assert_eq!(f.store.view(&key).unwrap().admission, Some(publication));
    }
}

/// Scenario: Forget a queued key, prune its journal and recreate it with a newer queue. Reload between every step and crash before and after recreation; the old completion stays fenced and the new check can pass.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_recreation_after_forget_fences_old_ticket() {
    recreation(false);
}

/// Scenario: Trip a queued key, prune its journal and recreate it with a newer queue. Reload between every step and crash before and after recreation; the old completion stays fenced and the new check can pass.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_recreation_after_trip_fences_old_ticket() {
    recreation(true);
}

/// Scenario: Queue a fresh key after another journal raised the floor, then recover the queue's journal as absent without losing its durable ticket. Publication must create a floor-fenced journal before writing the passing result.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_publish_absent_journal_uses_creation_floor() {
    let mut f = Fixture::new();
    let old = key("0.0.1");
    f.store.queue(&old).unwrap();
    testing::prune_keys(&mut f.store, &[old]).unwrap();
    let key = key("1.0.0");
    let ticket = f.store.queue(&key).unwrap();
    // Models a missing journal with a still-current q > floor. It does not
    // call prune, since prune would intentionally raise floor through q.
    testing::remove_journal_without_pruning(&mut f.store, &key).unwrap();
    f.reload(CrashImage::KeepUnsynced);
    let floor = f.store.metadata().unwrap().floor;
    assert!(ticket.queued > floor);
    let point = Checkpoint::CreateJournalDurable;
    testing::arm(&mut f.store, Fault::CrashAt(point));
    crashed(
        f.store
            .publish(&ticket, evidence(Outcome::Passed), IMAGE)
            .map(|_| ()),
        point,
    );
    f.reload(CrashImage::KeepUnsynced);
    let created = journal(&f.store, &key);
    assert_eq!(created.last_event, floor);
    assert_eq!(created.revoked_through, floor);
    assert_eq!(created.state, None);
    assert!(created.ringing.is_empty());
    let p = published(
        f.store
            .publish(&ticket, evidence(Outcome::Passed), IMAGE)
            .unwrap(),
    );
    assert_eq!(f.store.view(&key).unwrap().admission, Some(p));
}

/// Scenario: Attempt a ring for a key with no journal. It is refused and creates neither a journal nor evidence or an epoch event.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_ring_absent_journal_creates_nothing() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    let before = f.store.metadata().unwrap();
    assert!(f.store.ring_start(&key, &mark("missing")).is_err());
    let view = f.store.view(&key).unwrap();
    assert!(view.journal.is_none());
    assert!(view.evidence.is_none());
    assert_eq!(f.store.metadata().unwrap(), before);
}

/// Scenario: Trip a measured key, then pass a newer check, publish Inconclusive, evict evidence and convert a dead owner. Each result preserves at least the original revocation watermark after reload.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_watermark_survives_successor_states() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    f.pass(&key);
    let trip = f.trip(&key);
    f.pass(&key);
    assert!(journal(&f.store, &key).revoked_through >= trip);
    // Leave one durable mark while its owner is still live; a later
    // Inconclusive must not retire it, so conversion remains reachable.
    f.store.ring_start(&key, &mark("later-dead")).unwrap();
    let ticket = f.store.queue(&key).unwrap();
    published(
        f.store
            .publish(
                &ticket,
                evidence(Outcome::Inconclusive {
                    reason: "limit: trace".into(),
                }),
                IMAGE,
            )
            .unwrap(),
    );
    f.reload(CrashImage::KeepUnsynced);
    assert!(journal(&f.store, &key).revoked_through >= trip);
    assert_eq!(journal(&f.store, &key).ringing, vec![mark("later-dead")]);
    f.store.evict_evidence(&key).unwrap();
    f.reload(CrashImage::KeepUnsynced);
    assert!(journal(&f.store, &key).revoked_through >= trip);
    testing::owners(&mut f.store, Vec::new());
    f.store.convert_dead_owners(&key).unwrap();
    f.reload(CrashImage::KeepUnsynced);
    assert!(journal(&f.store, &key).revoked_through >= trip);
}

/// Scenario: Run seeded random sequences of all transition rows, with a crash at a seeded write boundary and reloads after every action. The watermark never decreases, including across pruning and recreation; failures print the replay seed.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_watermark_random_rows_random_crash() {
    for seed in [
        0x5343_0301_u64,
        0x5343_0302,
        0x5343_0303,
        0x5343_0304,
        0x5343_0305,
        0x5343_0306,
        0x5343_0307,
        0x5343_0308,
    ] {
        let result = std::panic::catch_unwind(|| random_rows(seed));
        assert!(
            result.is_ok(),
            "watermark property failed; replay seed={seed:#018x}"
        );
    }
}

/// Scenario: Submit a passing result with a different runner image or publishing daemon image. Both are discarded; the matching result remains publishable.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_result_image_hash_must_match_key_and_daemon() {
    let mut f = Fixture::new();
    let key = key("1.0.0");
    let ticket = f.store.queue(&key).unwrap();
    let mut changed = evidence(Outcome::Passed);
    changed.runner_image_hash = "e".repeat(64);
    assert_eq!(
        f.store.publish(&ticket, changed, IMAGE).unwrap(),
        Publication::Stale
    );
    assert_eq!(
        f.store
            .publish(&ticket, evidence(Outcome::Passed), &"e".repeat(64))
            .unwrap(),
        Publication::Stale
    );
    published(
        f.store
            .publish(&ticket, evidence(Outcome::Passed), IMAGE)
            .unwrap(),
    );
}

/// Scenario: Queue valid boundary-length release names and reject traversal, unsupported agents and malformed hashes before any file is written. The tempdir store and its files have private modes.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_validated_names_and_private_modes() {
    validation_controls();
}

/// Scenario: Publish over twenty results for one agent with older passing and newer nonpassing evidence. Retention removes the oldest nonpassing evidence first while every journal and watermark remains.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_evidence_retention_keeps_journals() {
    let mut f = Fixture::new();
    let mut keys = Vec::new();
    for i in 0..22 {
        let key = key(&format!("1.0.{i}"));
        let ticket = f.store.queue(&key).unwrap();
        let outcome = if i == 0 || i >= 20 {
            Outcome::Passed
        } else {
            Outcome::Inconclusive {
                reason: "fixture".into(),
            }
        };
        published(f.store.publish(&ticket, evidence(outcome), IMAGE).unwrap());
        keys.push(key);
    }
    f.store.retain_evidence(20).unwrap();
    assert!(f.store.view(&keys[0]).unwrap().evidence.is_some());
    assert!(f.store.view(&keys[1]).unwrap().evidence.is_none());
    assert!(f.store.view(&keys[2]).unwrap().evidence.is_none());
    assert!(f.store.view(&keys[3]).unwrap().evidence.is_some());
    let count = keys
        .iter()
        .filter(|key| f.store.view(key).unwrap().evidence.is_some())
        .count();
    assert_eq!(count, 20);
    for key in keys {
        assert!(f.store.view(&key).unwrap().journal.is_some());
    }
}

/// Scenario: Create three journals in event order, keep the oldest marked, and prune to two. The oldest unmarked journal is removed while the mark and newest journal survive.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_prune_oldest_event_skips_marks() {
    let mut f = Fixture::new();
    let keys = [key("1.0.0"), key("2.0.0"), key("3.0.0")];
    for key in &keys {
        f.pass(key);
    }
    f.store.ring_start(&keys[0], &mark("keep")).unwrap();
    f.store.prune_to(2).unwrap();
    assert!(f.store.view(&keys[0]).unwrap().journal.is_some());
    assert!(f.store.view(&keys[1]).unwrap().journal.is_none());
    assert!(f.store.view(&keys[2]).unwrap().journal.is_some());
}

/// Scenario: Open two store handles and release two threads together to queue checks for the same key. Their epochs must be distinct and a later clean load must retain the maximum without any lost authority event.
#[test]
#[spec("inbox/doorbell/061")]
fn doorbell_061_store_lock_serializes_independent_handles() {
    let f = Fixture::new();
    let root = f.root();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = testing::open(&root, vec![owner(100)]).unwrap();
                barrier.wait();
                store.queue(&key("1.0.0")).unwrap().queued
            })
        })
        .collect();
    let mut epochs: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    epochs.sort();
    assert!(
        epochs[0] < epochs[1],
        "store lock lost an epoch: {epochs:?}"
    );
    let store = testing::open(&root, vec![owner(100)]).unwrap();
    assert_eq!(store.metadata().unwrap().epoch, epochs[1]);
    invariant(&store);
}
