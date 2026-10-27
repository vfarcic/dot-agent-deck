//! PRD #1666 part 3 (Q35-05): the arm page's two-word publication
//! (`src/composer_probe/arm.rs`). The daemon publishes `count` and `seg_seq`
//! with a seqlock; the helper reads them from one publication or reads the
//! arm as present. Most cases drive the writer and the reader through their
//! pause seams in one thread; the rest race them, in threads and across two
//! processes mapping the same file.

#![cfg(target_has_atomic = "64")]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::arm::{
    ArmRead, ArmReader, PageError, Publisher, READ_RETRIES, ReaderAt, Unread, WORDS_BYTES,
    WriterAt, availability, halves, word,
};
use spec::spec;

/// A page in this process, 8-byte aligned, as a page-aligned mapping is.
#[repr(C, align(4096))]
struct Page([AtomicU64; 512]);

impl Page {
    fn new(word0: u64, seg_seq: u64) -> Box<Page> {
        let page = Box::new(Page(std::array::from_fn(|_| AtomicU64::new(0))));
        page.0[0].store(word0, Ordering::Relaxed);
        page.0[1].store(seg_seq, Ordering::Relaxed);
        page
    }

    fn ptr(&self) -> *mut u8 {
        self.0.as_ptr().cast_mut().cast()
    }

    /// Word 0 and `seg_seq` as they are now.
    fn raw(&self) -> (u64, u64) {
        (
            self.0[0].load(Ordering::Relaxed),
            self.0[1].load(Ordering::Relaxed),
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

/// A `seg_seq` that no other count shares, with both halves moving as the
/// count does: a read pairing one publication's count with another's
/// `seg_seq` breaks it.
fn seg_for(count: u32) -> u64 {
    (u64::from(count) ^ 0x5bd1_e995).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// The `k`th count of a stress run, above 2¹⁶ for most `k`.
fn count_for(k: u64) -> u32 {
    (k as u32).wrapping_mul(0x9e37_79b9)
}

/// Scenario: Publish to a page in this process and read it back: a new, zeroed page reads count 0 and `seg_seq` 0, which is absent; a count above 2¹⁶ with a `seg_seq` above 2³², the largest count with the largest `seg_seq`, and count 0 with a later `seg_seq` each read back unchanged, word 0 holding the count in its high half and an even `seq` in its low half, at offsets 0 and 8; a count above zero is present, and count 0 absent. The platform has lock-free 64-bit atomics, so the page is available.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_a_publication_reads_back_whole() {
    assert_eq!(availability(), Ok(()));
    assert_eq!(WORDS_BYTES, 16);
    assert_eq!(halves(word(0x1_0001, 6)), (0x1_0001, 6));

    let page = Page::new(0, 0);
    let reader = page.reader();
    assert_eq!(
        reader.read(),
        ArmRead::Read {
            count: 0,
            seg_seq: 0
        }
    );
    assert!(!reader.read().present());

    let mut publisher = page.publisher();
    for (seq, count, seg_seq) in [
        (2, 70_000, (1 << 32) + 1),
        (4, u32::MAX, u64::MAX),
        (6, 0, 1 << 40),
        (8, 1, 7),
    ] {
        publisher.publish(count, seg_seq);
        assert_eq!(reader.read(), ArmRead::Read { count, seg_seq });
        assert_eq!(page.raw(), (word(count, seq), seg_seq));
        assert_eq!(reader.read().present(), count > 0);
    }
}

/// Scenario: Start a page at `seq` 2³² − 2 and publish twice: the odd store is 2³² − 1 with the old count still in word 0, `seg_seq` changes while word 0 is odd, and the even store wraps `seq` to 0, read consistent; the next odd store is 1 and carries the count just published, and its even store is 2. A page left mid-publication (an odd `seq`) reads as busy, and a publisher taking it up publishes with an even `seq`.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_seq_wraps_keeping_parity() {
    let page = Page::new(word(3, u32::MAX - 1), 10);
    let mut publisher = page.publisher();
    let mut seen = Vec::new();
    publisher.publish_pausing(9, 11, |at| seen.push((at, page.raw())));
    assert_eq!(
        seen,
        [
            (WriterAt::Odd, (word(3, u32::MAX), 10)),
            (WriterAt::SegSeq, (word(3, u32::MAX), 11)),
        ]
    );
    assert_eq!(page.raw(), (word(9, 0), 11));
    assert_eq!(
        page.reader().read(),
        ArmRead::Read {
            count: 9,
            seg_seq: 11
        }
    );
    let mut odd = None;
    publisher.publish_pausing(10, 12, |at| {
        if at == WriterAt::Odd {
            odd = Some(page.raw());
        }
    });
    assert_eq!(odd, Some((word(9, 1), 11)), "the count just published");
    assert_eq!(page.raw(), (word(10, 2), 12));

    let left = Page::new(word(4, 7), 20);
    assert_eq!(left.reader().read(), ArmRead::Unread(Unread::Busy));
    let mut publisher = left.publisher();
    publisher.publish(5, 21);
    assert_eq!(left.raw(), (word(5, 10), 21));
    assert_eq!(
        left.reader().read(),
        ArmRead::Read {
            count: 5,
            seg_seq: 21
        }
    );
}

/// Scenario: Park the writer after its odd store, and again after its `seg_seq` store before the final publication, and read the page from there: each read is busy and so present, though the count being published is 0. Once the publication completes, the read is consistent and absent.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_a_parked_writer_reads_present() {
    let page = Page::new(0, 0);
    let mut publisher = page.publisher();
    publisher.publish(2, 5);
    let reader = page.reader();
    let mut reads = Vec::new();
    publisher.publish_pausing(0, 6, |at| reads.push((at, reader.read())));
    assert_eq!(
        reads,
        [
            (WriterAt::Odd, ArmRead::Unread(Unread::Busy)),
            (WriterAt::SegSeq, ArmRead::Unread(Unread::Busy)),
        ]
    );
    assert!(reads.iter().all(|(_, read)| read.present()));
    assert_eq!(
        reader.read(),
        ArmRead::Read {
            count: 0,
            seg_seq: 6
        }
    );
}

/// Scenario: Pause the reader after its first load of word 0, and again after its `seg_seq` load, and publish a whole new value there: the read starts over and returns the new count and `seg_seq` together, never the old count with the new `seg_seq`. A publication at each of the first three tries still ends in a consistent read on the fourth; one at each of four tries is busy, and present.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_a_reader_paused_mid_read_starts_over() {
    for at in [ReaderAt::First, ReaderAt::SegSeq] {
        let page = Page::new(0, 0);
        let mut publisher = page.publisher();
        publisher.publish(1, 100);
        let reader = page.reader();
        let mut tries = 0;
        let read = reader.read_pausing(|now| {
            if now == at {
                tries += 1;
                if tries == 1 {
                    publisher.publish(70_000, 1 << 33);
                }
            }
        });
        assert_eq!(
            read,
            ArmRead::Read {
                count: 70_000,
                seg_seq: 1 << 33
            },
            "{at:?}"
        );
        assert_eq!(tries, 2, "{at:?}");
    }

    for (moving, expected) in [
        (
            READ_RETRIES,
            ArmRead::Read {
                count: 3,
                seg_seq: 3,
            },
        ),
        (READ_RETRIES + 1, ArmRead::Unread(Unread::Busy)),
    ] {
        let page = Page::new(0, 0);
        let mut publisher = page.publisher();
        let mut tries = 0;
        let read = page.reader().read_pausing(|now| {
            if now == ReaderAt::SegSeq {
                tries += 1;
                if tries <= moving {
                    publisher.publish(tries as u32, tries as u64);
                }
            }
        });
        assert_eq!(read, expected, "{moving} tries moved");
        assert!(read.present());
    }
}

/// Scenario: A page that can't be read: no mapping (a null pointer), `MAP_FAILED`, a page one byte off 8-byte alignment, and one shorter than the two words. Each reads as present, with its reason, and a publisher won't take a null, misaligned or short page; 16 aligned bytes read.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_a_page_that_cannot_be_read_is_present() {
    let page = Page::new(0, 0);
    let map_failed = usize::MAX as *mut u8;
    // SAFETY: none of these is read, as `at` checks before it reads.
    let reads = unsafe {
        [
            (
                PageError::Unmapped,
                ArmReader::read_at(std::ptr::null(), 4096),
            ),
            (PageError::Misaligned, ArmReader::read_at(map_failed, 4096)),
            (
                PageError::Misaligned,
                ArmReader::read_at(page.ptr().add(1), 4095),
            ),
            (
                PageError::Short,
                ArmReader::read_at(page.ptr(), WORDS_BYTES - 1),
            ),
        ]
    };
    for (error, read) in reads {
        assert_eq!(read, ArmRead::Unread(Unread::Page(error)));
        assert!(read.present(), "{error:?}");
    }
    // SAFETY: as above.
    unsafe {
        assert_eq!(
            Publisher::at(std::ptr::null_mut(), 4096).err(),
            Some(PageError::Unmapped)
        );
        assert_eq!(
            Publisher::at(page.ptr().add(4), 4092).err(),
            Some(PageError::Misaligned)
        );
        assert_eq!(
            Publisher::at(page.ptr(), WORDS_BYTES - 1).err(),
            Some(PageError::Short)
        );
        assert_eq!(
            ArmReader::read_at(page.ptr(), WORDS_BYTES),
            ArmRead::Read {
                count: 0,
                seg_seq: 0
            }
        );
    }
}

/// What a stress reader saw.
#[derive(Debug, Default)]
struct Stress {
    consistent: usize,
    busy: usize,
    torn: usize,
    distinct: usize,
}

/// Read until `reads` consistent reads have seen `distinct` counts, or until
/// `limit` passes or a hundred reads for each one wanted are busy, checking
/// each against [`seg_for`]. The count to see change, not just the reads, is
/// what makes the reads race the writer: 20,000 reads can finish before a
/// loaded writer has published twice.
fn stress_read(reader: ArmReader<'_>, reads: usize, distinct: usize, limit: Duration) -> Stress {
    let started = Instant::now();
    let mut stress = Stress::default();
    let mut counts = HashSet::new();
    while (stress.consistent < reads || counts.len() < distinct)
        && stress.busy < reads * 100
        && started.elapsed() < limit
    {
        match reader.read() {
            ArmRead::Read { count, seg_seq } => {
                stress.consistent += 1;
                if seg_seq != seg_for(count) {
                    stress.torn += 1;
                }
                counts.insert(count);
            }
            ArmRead::Unread(_) => stress.busy += 1,
        }
    }
    stress.distinct = counts.len();
    stress
}

/// Publish [`count_for`]`(k)` with its [`seg_for`] for `k` = 1, 2, … until
/// `stop`.
fn stress_publish(publisher: &mut Publisher<'_>, stop: &AtomicBool) -> u64 {
    let mut k = 0;
    while !stop.load(Ordering::Relaxed) {
        k += 1;
        let count = count_for(k);
        publisher.publish(count, seg_for(count));
        for _ in 0..(k % 64) {
            std::hint::spin_loop();
        }
    }
    k
}

/// Scenario: Race one writer thread, publishing counts mostly above 2¹⁶ each with a `seg_seq` above 2³² that only that count has, against two reader threads, until each has made 20,000 consistent reads and seen 100 counts (or 10 seconds pass). Each does both, and no consistent read pairs one publication's count with another's `seg_seq`.
#[spec("inbox/probe/007")]
#[test]
fn probe_007_threads_never_read_a_torn_pair() {
    let page = Page::new(word(count_for(0), 0), seg_for(count_for(0)));
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
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    use dot_agent_deck::composer_probe::arm::{ArmRead, ArmReader, Publisher, WriterAt, word};
    use spec::spec;

    use super::{count_for, seg_for, stress_publish, stress_read};

    const CHILD: &str = "DAD_ARM_PAGE_CHILD";
    /// What starts each of the child's answers. libtest prints the test's
    /// name with no newline before the test runs, so an answer can follow
    /// it on one line.
    const ANSWER: &str = "arm-child> ";
    const PAGE: usize = 4096;

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

    /// The child: map the page read-only and answer the parent's commands
    /// on standard input until it closes.
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
                ["read"] => match reader.read() {
                    ArmRead::Read { count, seg_seq } => println!("{ANSWER}read {count} {seg_seq}"),
                    ArmRead::Unread(why) => println!("{ANSWER}unread {why:?}"),
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

    /// The parent's end of the child.
    struct Server {
        child: Child,
        stdin: Option<ChildStdin>,
        stdout: BufReader<ChildStdout>,
    }

    impl Server {
        fn ask(&mut self, command: &str) -> String {
            let stdin = self.stdin.as_mut().unwrap();
            writeln!(stdin, "{command}").unwrap();
            stdin.flush().unwrap();
            self.answer()
        }

        /// The child's next answer, past libtest's own output.
        fn answer(&mut self) -> String {
            loop {
                let mut line = String::new();
                assert_ne!(
                    self.stdout.read_line(&mut line).unwrap(),
                    0,
                    "the child ended"
                );
                if let Some((_, answer)) = line.trim_end().split_once(ANSWER) {
                    return answer.to_owned();
                }
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Scenario: The test maps a page-long file shared and writable, starts this test again as a child that maps the same file read-only, and drives the child's reads over a pipe. The child reads a new page (count 0, `seq` at 2³² − 2) as absent; after a publication of a count above 2¹⁶ with a `seg_seq` above 2³², whose `seq` wraps to 0, it reads both unchanged; with the writer parked after its odd store, and after its `seg_seq` store, it reads busy, which is present; after that publication completes, it reads the new pair. Then the parent publishes as fast as it can while the child reads until it has made 20,000 consistent reads and seen 100 counts, which it does, none of its reads pairing one publication's count with another's `seg_seq`.
    #[spec("inbox/probe/007")]
    #[test]
    fn probe_007_two_processes_share_the_page() {
        if let Some(path) = std::env::var_os(CHILD) {
            serve(Path::new(&path));
            return;
        }
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

        let name = "processes::probe_007_two_processes_share_the_page";
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut server = Server {
            stdin: child.stdin.take(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
        };

        assert_eq!(server.ask("read"), "read 0 0");
        publisher.publish(70_000, (1 << 33) + 5);
        assert_eq!(server.ask("read"), "read 70000 8589934597");
        let mut parked = Vec::new();
        publisher.publish_pausing(1, 7, |at| parked.push((at, server.ask("read"))));
        assert_eq!(
            parked,
            [
                (WriterAt::Odd, "unread Busy".to_owned()),
                (WriterAt::SegSeq, "unread Busy".to_owned()),
            ]
        );
        assert_eq!(server.ask("read"), "read 1 7");

        // Every pair the stress reads must satisfy `seg_for`, the first too.
        publisher.publish(count_for(0), seg_for(count_for(0)));
        let stop = AtomicBool::new(false);
        let answer = std::thread::scope(|scope| {
            let writer = scope.spawn(|| stress_publish(&mut publisher, &stop));
            let answer = server.ask("stress 20000 100 10");
            stop.store(true, Ordering::Relaxed);
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
