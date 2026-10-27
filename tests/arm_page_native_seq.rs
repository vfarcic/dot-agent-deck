//! PRD #1666 part 3 (Q35-13, amendment 2 §4.4): the arm page's third word,
//! `native_seq` (`src/composer_probe/arm.rs`). The daemon publishes it under
//! the same seqlock as the count and `seg_seq`; the helper reads all three
//! from one publication or reads the arm as present. The two-word cases are
//! `tests/arm_page.rs`; these drive the three-word publisher and reader the
//! same ways: through their pause seams in one thread, raced in threads, and
//! across two processes mapping one file.

#![cfg(target_has_atomic = "64")]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::arm::{
    ArmRead, ArmReader, Publication, Publisher, READ_RETRIES, ReaderAt, Unread, WORDS_BYTES,
    WriterAt, word,
};
use spec::spec;

/// A page in this process, 8-byte aligned, as a page-aligned mapping is.
#[repr(C, align(4096))]
struct Page([AtomicU64; 512]);

impl Page {
    fn new(word0: u64, seg_seq: u64, native_seq: u64) -> Box<Page> {
        let page = Box::new(Page(std::array::from_fn(|_| AtomicU64::new(0))));
        page.0[0].store(word0, Ordering::Relaxed);
        page.0[1].store(seg_seq, Ordering::Relaxed);
        page.0[2].store(native_seq, Ordering::Relaxed);
        page
    }

    fn ptr(&self) -> *mut u8 {
        self.0.as_ptr().cast_mut().cast()
    }

    /// Word 0, `seg_seq` and `native_seq` as they are now, at offsets 0, 8
    /// and 16.
    fn raw(&self) -> (u64, u64, u64) {
        (
            self.0[0].load(Ordering::Relaxed),
            self.0[1].load(Ordering::Relaxed),
            self.0[2].load(Ordering::Relaxed),
        )
    }

    fn reader(&self) -> ArmReader<'_> {
        // SAFETY: the page lives as long as the reader, and every access to
        // its words is atomic.
        unsafe { ArmReader::at(self.ptr(), size_of::<Page>()) }.unwrap()
    }

    fn publisher(&self) -> Publisher<'_> {
        // SAFETY: as `reader`, and each test makes one publisher at a time.
        unsafe { Publisher::at(self.ptr(), size_of::<Page>()) }.unwrap()
    }
}

fn publication(count: u32, seg_seq: u64, native_seq: u64) -> Publication {
    Publication {
        count,
        seg_seq,
        native_seq,
    }
}

/// A `seg_seq` only `count` has.
fn seg_for(count: u32) -> u64 {
    (u64::from(count) ^ 0x5bd1_e995).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// A `native_seq` only `count` has, unrelated to its [`seg_for`], with every
/// byte of the word moving.
fn native_for(count: u32) -> u64 {
    (u64::from(count) ^ 0xc2b2_ae35).wrapping_mul(0xff51_afd7_ed55_8ccd) | 1 << 63
}

/// The `k`th count of a stress run.
fn count_for(k: u64) -> u32 {
    (k as u32).wrapping_mul(0x9e37_79b9)
}

/// Scenario: Publish full 64-bit `native_seq` values (the largest, the top bit alone, and one with every byte different) with counts and segment numbers, and read them back: each read returns the three values one publication stored, and the page holds `native_seq` at offset 16 beside word 0 and `seg_seq`. A new page reads `native_seq` 0.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_native_seq_reads_back_whole_at_offset_16() {
    assert_eq!(WORDS_BYTES, 24);
    let page = Page::new(0, 0, 0);
    let reader = page.reader();
    assert_eq!(reader.read_publication(), Ok(publication(0, 0, 0)));

    let mut publisher = page.publisher();
    for (seq, count, seg_seq, native_seq) in [
        (2, 70_000, (1 << 32) + 1, u64::MAX),
        (4, u32::MAX, u64::MAX, 1 << 63),
        (6, 0, 1 << 40, 0x0123_4567_89ab_cdef),
        (8, 1, 7, 0),
    ] {
        publisher.publish_native(count, seg_seq, native_seq);
        assert_eq!(
            reader.read_publication(),
            Ok(publication(count, seg_seq, native_seq))
        );
        assert_eq!(page.raw(), (word(count, seq), seg_seq, native_seq));
        assert_eq!(reader.read(), ArmRead::Read { count, seg_seq });
    }
}

/// Scenario: Change one word at a time. A three-word publication that moves only `native_seq` leaves the count and `seg_seq` as they were; a two-word publication moves the count and `seg_seq` and leaves `native_seq` as the last three-word publication stored it, never storing it, so a value written past the publisher stays; and a three-word publication that moves only `seg_seq` leaves the other two.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_segment_and_native_words_change_independently() {
    let page = Page::new(0, 0, 0);
    let reader = page.reader();
    let mut publisher = page.publisher();
    publisher.publish_native(3, 10, 100);
    publisher.publish_native(3, 10, 101);
    assert_eq!(reader.read_publication(), Ok(publication(3, 10, 101)));
    publisher.publish(4, 11);
    assert_eq!(reader.read_publication(), Ok(publication(4, 11, 101)));
    publisher.publish_native(4, 12, 101);
    assert_eq!(reader.read_publication(), Ok(publication(4, 12, 101)));

    // A two-word publication doesn't store the third word at all.
    page.0[2].store(555, Ordering::Relaxed);
    publisher.publish(5, 13);
    assert_eq!(page.raw().2, 555);
    assert_eq!(reader.read_publication(), Ok(publication(5, 13, 555)));
}

/// Scenario: Park the three-word writer after its odd store, after its `seg_seq` store and after its `native_seq` store, and read all three words there: each read is busy and present though the count being published is 0, the page shows each word stored in turn, and once the publication completes all three read back together. A two-word publication pauses only after its odd store and its `seg_seq` store.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_a_writer_parked_at_each_word_reads_busy() {
    let page = Page::new(0, 0, 0);
    let mut publisher = page.publisher();
    publisher.publish_native(2, 5, 50);
    let reader = page.reader();
    let mut seen = Vec::new();
    publisher.publish_native_pausing(0, 6, 60, |at| {
        seen.push((at, page.raw(), reader.read_publication()));
    });
    assert_eq!(
        seen,
        [
            (WriterAt::Odd, (word(2, 3), 5, 50), Err(Unread::Busy)),
            (WriterAt::SegSeq, (word(2, 3), 6, 50), Err(Unread::Busy)),
            (WriterAt::NativeSeq, (word(2, 3), 6, 60), Err(Unread::Busy)),
        ]
    );
    assert!(ArmRead::from(Err(Unread::Busy)).present());
    assert_eq!(reader.read_publication(), Ok(publication(0, 6, 60)));
    assert!(!reader.read().present());

    let mut points = Vec::new();
    publisher.publish_pausing(1, 7, |at| points.push(at));
    assert_eq!(points, [WriterAt::Odd, WriterAt::SegSeq]);
}

/// Scenario: Pause the three-word reader after its first load of word 0, after its `seg_seq` load and after its `native_seq` load, and land a whole publication there that moves all three words: the read starts over and returns the new count, `seg_seq` and `native_seq` together, never a mix. A publication at each of the first three tries still reads consistently on the fourth; one at each of four tries is busy.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_a_reader_paused_at_each_word_starts_over() {
    for at in [ReaderAt::First, ReaderAt::SegSeq, ReaderAt::NativeSeq] {
        let page = Page::new(0, 0, 0);
        let mut publisher = page.publisher();
        publisher.publish_native(1, 100, 1000);
        let reader = page.reader();
        let mut tries = 0;
        let mut points = Vec::new();
        let read = reader.read_publication_pausing(|now| {
            points.push(now);
            if now == at {
                tries += 1;
                if tries == 1 {
                    publisher.publish_native(70_000, 1 << 33, u64::MAX - 1);
                }
            }
        });
        assert_eq!(
            read,
            Ok(publication(70_000, 1 << 33, u64::MAX - 1)),
            "{at:?}"
        );
        assert_eq!(tries, 2, "{at:?}");
        assert_eq!(
            &points[points.len() - 3..],
            [ReaderAt::First, ReaderAt::SegSeq, ReaderAt::NativeSeq],
            "{at:?}"
        );
    }

    for (moving, expected) in [
        (READ_RETRIES, Ok(publication(3, 3, 3))),
        (READ_RETRIES + 1, Err(Unread::Busy)),
    ] {
        let page = Page::new(0, 0, 0);
        let mut publisher = page.publisher();
        let mut tries = 0;
        let read = page.reader().read_publication_pausing(|now| {
            if now == ReaderAt::NativeSeq {
                tries += 1;
                if tries <= moving {
                    publisher.publish_native(tries as u32, tries as u64, tries as u64);
                }
            }
        });
        assert_eq!(read, expected, "{moving} tries moved");
    }
}

/// Scenario: Start a page at `seq` 2³² − 2 holding the largest `seg_seq` and `native_seq` and publish three words twice: `seq` wraps to 0 and then reaches 2, the odd stores keep the count before them, and both 64-bit words read back at and across their largest values, which the page carries as given and never wraps.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_native_seq_across_the_seq_wrap() {
    let page = Page::new(word(3, u32::MAX - 1), u64::MAX, u64::MAX);
    let reader = page.reader();
    assert_eq!(
        reader.read_publication(),
        Ok(publication(3, u64::MAX, u64::MAX))
    );
    let mut publisher = page.publisher();
    let mut odd = Vec::new();
    publisher.publish_native_pausing(9, u64::MAX, u64::MAX, |at| {
        if at == WriterAt::Odd {
            odd.push(page.raw());
        }
    });
    assert_eq!(odd, [(word(3, u32::MAX), u64::MAX, u64::MAX)]);
    assert_eq!(page.raw(), (word(9, 0), u64::MAX, u64::MAX));
    assert_eq!(
        reader.read_publication(),
        Ok(publication(9, u64::MAX, u64::MAX))
    );
    publisher.publish_native(10, 0, u64::MAX - 1);
    assert_eq!(page.raw(), (word(10, 2), 0, u64::MAX - 1));
    assert_eq!(
        reader.read_publication(),
        Ok(publication(10, 0, u64::MAX - 1))
    );

    let left = Page::new(word(4, 7), 20, 200);
    assert_eq!(left.reader().read_publication(), Err(Unread::Busy));
    left.publisher().publish_native(5, 21, 201);
    assert_eq!(left.raw(), (word(5, 10), 21, 201));
}

/// What a stress reader saw.
#[derive(Debug, Default)]
struct Stress {
    consistent: usize,
    busy: usize,
    torn: usize,
    distinct: usize,
}

/// Read three words until `reads` consistent reads have seen `distinct`
/// counts, or `limit` passes or a hundred reads per wanted one are busy,
/// checking each `seg_seq` and `native_seq` against the count's.
fn stress_read(reader: ArmReader<'_>, reads: usize, distinct: usize, limit: Duration) -> Stress {
    let started = Instant::now();
    let mut stress = Stress::default();
    let mut counts = HashSet::new();
    while (stress.consistent < reads || counts.len() < distinct)
        && stress.busy < reads * 100
        && started.elapsed() < limit
    {
        match reader.read_publication() {
            Ok(p) => {
                stress.consistent += 1;
                if p.seg_seq != seg_for(p.count) || p.native_seq != native_for(p.count) {
                    stress.torn += 1;
                }
                counts.insert(p.count);
            }
            Err(_) => stress.busy += 1,
        }
    }
    stress.distinct = counts.len();
    stress
}

/// Publish [`count_for`]`(k)` with its words for `k` = 1, 2, … until `stop`.
fn stress_publish(publisher: &mut Publisher<'_>, stop: &AtomicBool) -> u64 {
    let mut k = 0;
    while !stop.load(Ordering::Relaxed) {
        k += 1;
        let count = count_for(k);
        publisher.publish_native(count, seg_for(count), native_for(count));
        for _ in 0..(k % 64) {
            std::hint::spin_loop();
        }
    }
    k
}

/// Scenario: Race one writer thread publishing three words, each count with a `seg_seq` and a `native_seq` only it has, against two reader threads, until each has made 20,000 consistent reads and seen 100 counts (or 10 seconds pass). Each does both, and no consistent read mixes words from two publications.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_threads_never_read_a_torn_triple() {
    let first = count_for(0);
    let page = Page::new(word(first, 0), seg_for(first), native_for(first));
    let mut publisher = page.publisher();
    let stop = AtomicBool::new(false);
    let (published, stresses) = std::thread::scope(|scope| {
        let writer = scope.spawn(|| stress_publish(&mut publisher, &stop));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| stress_read(page.reader(), 20_000, 100, Duration::from_secs(10)))
            })
            .collect();
        let stresses: Vec<Stress> = readers.into_iter().map(|r| r.join().unwrap()).collect();
        stop.store(true, Ordering::Relaxed);
        (writer.join().unwrap(), stresses)
    });
    eprintln!("published {published}: {stresses:?}");
    for stress in stresses {
        assert_eq!(stress.torn, 0, "{stress:?}");
        assert!(stress.consistent >= 20_000, "{stress:?}");
        assert!(stress.distinct >= 100, "{stress:?}");
    }
}

/// Two processes mapping one file, as the daemon and its helper do.
#[cfg(unix)]
mod processes {
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::fd::AsRawFd as _;
    use std::path::Path;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Duration;

    use dot_agent_deck::composer_probe::arm::{ArmReader, Publisher, WriterAt, word};
    use spec::spec;

    use super::{count_for, native_for, seg_for, stress_publish, stress_read};

    const CHILD: &str = "DAD_ARM_PAGE_NATIVE_CHILD";
    /// What starts each of the child's answers.
    const ANSWER: &str = "arm-native-child> ";
    const PAGE: usize = 4096;
    const ANSWER_WAIT: Duration = Duration::from_secs(60);

    /// A shared mapping of a page-long file.
    struct Mapping(*mut u8);

    impl Mapping {
        fn new(file: &File, prot: libc::c_int) -> Mapping {
            // SAFETY: a fresh shared mapping of an open file at least `PAGE`
            // long; the result is checked.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    PAGE,
                    prot,
                    libc::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(ptr, libc::MAP_FAILED, "{}", std::io::Error::last_os_error());
            Mapping(ptr.cast())
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: the mapping `new` made, unmapped once.
            unsafe { libc::munmap(self.0.cast(), PAGE) };
        }
    }

    /// The child: map the page read-only and answer the parent's commands.
    fn serve(path: &Path) {
        let file = File::open(path).unwrap();
        let mapping = Mapping::new(&file, libc::PROT_READ);
        // SAFETY: the mapping lives until the end of this function, and the
        // parent only stores to the words atomically.
        let reader = unsafe { ArmReader::at(mapping.0, PAGE) }.unwrap();
        for line in std::io::stdin().lines() {
            let line = line.unwrap();
            let words: Vec<&str> = line.split_whitespace().collect();
            match words.as_slice() {
                ["read"] => match reader.read_publication() {
                    Ok(p) => println!("{ANSWER}read {} {} {}", p.count, p.seg_seq, p.native_seq),
                    Err(why) => println!("{ANSWER}unread {why:?}"),
                },
                ["stress", reads, distinct, secs] => {
                    let stress = stress_read(
                        reader,
                        reads.parse().unwrap(),
                        distinct.parse().unwrap(),
                        Duration::from_secs(secs.parse().unwrap()),
                    );
                    println!(
                        "{ANSWER}stress {} {} {} {}",
                        stress.consistent, stress.busy, stress.torn, stress.distinct
                    );
                }
                _ => panic!("unknown command {line:?}"),
            }
        }
        println!("{ANSWER}done");
    }

    struct Server {
        child: Child,
        stdin: Option<ChildStdin>,
        answers: Receiver<String>,
    }

    impl Server {
        fn ask(&mut self, command: &str) -> String {
            let stdin = self.stdin.as_mut().unwrap();
            writeln!(stdin, "{command}").unwrap();
            stdin.flush().unwrap();
            self.answer()
        }

        fn answer(&mut self) -> String {
            match self.answers.recv_timeout(ANSWER_WAIT) {
                Ok(answer) => answer,
                Err(RecvTimeoutError::Disconnected) => panic!("the child ended"),
                Err(RecvTimeoutError::Timeout) => panic!("no answer in {ANSWER_WAIT:?}"),
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Stops the stress writer when dropped.
    struct StopOnDrop<'a>(&'a AtomicBool);

    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Scenario: The test maps a page-long file shared and writable and reruns itself as a child that maps the same file read-only, driving its reads over a pipe. The child reads three words: a new page (`seq` at 2³² − 2) as zero; a publication with the largest `native_seq`, whose `seq` wraps, unchanged; the writer parked after each word's store as busy; the completed publication; and, against a writer publishing as fast as it can, 20,000 consistent reads over 100 counts, none mixing words from two publications.
    #[spec("inbox/probe/007")]
    #[test]
    fn probe_007_two_processes_share_the_three_words() {
        if let Some(path) = std::env::var_os(CHILD) {
            serve(Path::new(&path));
            return;
        }
        std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let dir = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let path = dir.path().join("arm");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len(PAGE as u64).unwrap();
        let mapping = Mapping::new(&file, libc::PROT_READ | libc::PROT_WRITE);
        // SAFETY: the mapping is page-aligned and outlives this use; nothing
        // else writes it yet.
        unsafe { AtomicU64::from_ptr(mapping.0.cast()) }
            .store(word(0, u32::MAX - 1), Ordering::Relaxed);
        // SAFETY: the mapping outlives the publisher, which is its only
        // writer, and the child only loads the words atomically.
        let mut publisher = unsafe { Publisher::at(mapping.0, PAGE) }.unwrap();

        let name = "processes::probe_007_two_processes_share_the_three_words";
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, answers) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { return };
                if let Some((_, answer)) = line.trim_end().split_once(ANSWER)
                    && sender.send(answer.to_owned()).is_err()
                {
                    return;
                }
            }
        });
        let mut server = Server {
            stdin: child.stdin.take(),
            answers,
            child,
        };

        assert_eq!(server.ask("read"), "read 0 0 0");
        publisher.publish_native(70_000, (1 << 33) + 5, u64::MAX);
        assert_eq!(
            server.ask("read"),
            format!("read 70000 8589934597 {}", u64::MAX)
        );
        let mut parked = Vec::new();
        publisher.publish_native_pausing(1, 7, 1 << 63, |at| parked.push((at, server.ask("read"))));
        assert_eq!(
            parked,
            [
                (WriterAt::Odd, "unread Busy".to_owned()),
                (WriterAt::SegSeq, "unread Busy".to_owned()),
                (WriterAt::NativeSeq, "unread Busy".to_owned()),
            ]
        );
        assert_eq!(server.ask("read"), format!("read 1 7 {}", 1u64 << 63));

        let first = count_for(0);
        publisher.publish_native(first, seg_for(first), native_for(first));
        let stop = AtomicBool::new(false);
        let answer = std::thread::scope(|scope| {
            let writer = scope.spawn(|| stress_publish(&mut publisher, &stop));
            let stopping = StopOnDrop(&stop);
            let answer = server.ask("stress 20000 100 10");
            drop(stopping);
            let published = writer.join().unwrap();
            eprintln!("published {published}: {answer}");
            answer
        });
        let fields: Vec<usize> = answer
            .split_whitespace()
            .skip(1)
            .map(|field| field.parse().unwrap())
            .collect();
        let [consistent, _busy, torn, distinct] = fields[..] else {
            panic!("{answer}");
        };
        assert_eq!(torn, 0, "{answer}");
        assert!(consistent >= 20_000, "{answer}");
        assert!(distinct >= 100, "{answer}");

        drop(server.stdin.take());
        assert_eq!(server.answer(), "done");
        let status = server.child.wait().unwrap();
        assert!(status.success(), "{status}");
    }
}
