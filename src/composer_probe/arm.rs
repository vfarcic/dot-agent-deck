//! PRD #1666 part 3 (Q35-05, Q35-13; R5 §3.5, core §4.5, amendment 2
//! §4.4): the arm page, through which the daemon tells its editor helper
//! whether a deck Ctrl+G may be pending (the liability count), which segment
//! it last triggered (`seg_seq`) and which native Ctrl+G candidate it last
//! numbered (`native_seq`). The daemon is the page's only writer
//! ([`Publisher`]); the helper maps it read-only and reads it once per
//! invocation ([`ArmReader`]). Q35-13's installer creates and maps the page
//! ([`crate::composer_probe::instance`]); Q35-12's helper reads it, and
//! Q35-16 onwards publish to it.
//!
//! **The layout** ([`WORDS_BYTES`] at the start of the page, which is
//! page-aligned, so every word is 8-byte aligned):
//! - offset 0, **word 0**: one `AtomicU64` packing `count` (the high 32
//!   bits) and `seq` (the low 32), always loaded and stored whole ([`word`]);
//! - offset 8, **`seg_seq`**: a second `AtomicU64`, the segment number,
//!   never truncated;
//! - offset 16, **`native_seq`**: a third `AtomicU64`, the number of the
//!   last native candidate the daemon accepted, never truncated. A publisher
//!   that leaves it unchanged ([`Publisher::publish`]) doesn't store it.
//!
//! What follows is the daemon's own. A page shorter than the three words,
//! the 16-byte page of the two-word layout among them, can't be read or
//! published ([`PageError::Short`]).
//!
//! **Publishing** (one writer): store word 0 = `(count_old, seq + 1)`, odd
//! (Relaxed); a Release fence; store `seg_seq`, and `native_seq` when it
//! changes (Relaxed); store word 0 = `(count_new, seq + 2)`, even (Release).
//! **Reading:** load word 0 (Acquire), and start again if its `seq` is odd;
//! load `seg_seq`, then `native_seq` (Relaxed); an Acquire fence; load word
//! 0 again (Relaxed), and start again if it differs from the first load. A
//! read that isn't consistent after [`READ_RETRIES`] retries is
//! [`Unread::Busy`].
//!
//! **Why each ordering** (the seqlock of Boehm, "Can seqlocks get along with
//! programming language memory models?", 2012). Say the first load reads
//! publication `k`'s even store. That store is Release and the load
//! Acquire, so `k`'s data stores happen before the data loads, which
//! therefore read `k`'s values or later ones. Say either reads a later
//! publication `j`'s. `j`'s odd store is sequenced before its Release
//! fence, which is sequenced before its data stores; the reader's Acquire
//! fence follows a load that read one of those stores, so the fences
//! synchronize, and `j`'s odd store happens before the second load of word 0.
//! By coherence, that load reads `j`'s odd store or a later one, never `k`'s
//! even store again, so the two loads differ and the read starts over. The
//! two loads being equal and even therefore means both data words are `k`'s
//! (for a word `k` didn't store, the value the last publication to store it
//! left), and `count`, being in the same word, is `k`'s already. Relaxed
//! data stores and loads suffice because the fences and the Release/Acquire
//! pair carry the order. The one hole is `seq` coming back to the same value:
//! that takes 2³¹ publications between the reader's two loads of word 0, and
//! only parity and equality of `seq` are read, so its wrap at 2³² is
//! otherwise harmless. `seg_seq` and `native_seq` are never wrapped by the
//! page: they are the values the daemon gives it, and the daemon stops
//! numbering before either would wrap (amendment 3 §6.1). Tests supplement
//! this argument; they don't replace it.
//!
//! **What the helper concludes** (R5 §2.4): a consistent read with
//! `count == 0` is absent; `count > 0`, and anything that isn't a consistent
//! read (a page that isn't mapped, is misaligned or is too short, or a
//! writer that kept moving), is present ([`ArmRead::present`]). The file not
//! existing at all is the caller's to read as absent.
//!
//! **Where it can be used.** The page relies on 64-bit atomics that are
//! lock-free, and so don't depend on a lock in one process's memory, on a
//! mapping two processes share. Rust provides `AtomicU64` only where the
//! target has 64-bit atomics, and guarantees every atomic type it provides is
//! lock-free, so `cfg(target_has_atomic = "64")` is the check; there is no
//! further runtime question to ask. Elsewhere [`availability`] says
//! [`Unavailable::Platform`], and the observer is unavailable there.

/// The three words' size: word 0, `seg_seq` and `native_seq`.
pub const WORDS_BYTES: usize = 24;

/// How many times a read starts over before it gives up as [`Unread::Busy`].
pub const READ_RETRIES: usize = 3;

/// Why the arm page can't be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// No lock-free 64-bit atomics on this platform.
    Platform,
}

/// Whether this platform can use the arm page.
pub fn availability() -> Result<(), Unavailable> {
    if cfg!(target_has_atomic = "64") {
        Ok(())
    } else {
        Err(Unavailable::Platform)
    }
}

/// Word 0's value for `count` and `seq`.
pub const fn word(count: u32, seq: u32) -> u64 {
    ((count as u64) << 32) | seq as u64
}

/// Word 0's `(count, seq)`.
pub const fn halves(word: u64) -> (u32, u32) {
    ((word >> 32) as u32, word as u32)
}

/// Why a page can't be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageError {
    /// No mapping: a null pointer.
    Unmapped,
    /// Not 8-byte aligned (`MAP_FAILED` among them).
    Misaligned,
    /// Shorter than [`WORDS_BYTES`].
    Short,
}

/// Why a read isn't consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread {
    /// The page can't be read.
    Page(PageError),
    /// The writer was mid-publication, or published again, on every try.
    Busy,
}

/// One read of the page's count and `seg_seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmRead {
    /// A consistent read: `count` and `seg_seq` from one publication.
    Read { count: u32, seg_seq: u64 },
    /// No consistent read.
    Unread(Unread),
}

/// The three words of one publication: what a consistent
/// [`ArmReader::read_publication`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publication {
    pub count: u32,
    pub seg_seq: u64,
    pub native_seq: u64,
}

impl From<Result<Publication, Unread>> for ArmRead {
    fn from(read: Result<Publication, Unread>) -> Self {
        match read {
            Ok(p) => ArmRead::Read {
                count: p.count,
                seg_seq: p.seg_seq,
            },
            Err(why) => ArmRead::Unread(why),
        }
    }
}

impl ArmRead {
    /// Whether the helper must treat the arm as present: a count above
    /// zero, or no consistent read.
    pub fn present(&self) -> bool {
        !matches!(self, ArmRead::Read { count: 0, .. })
    }
}

/// Where [`Publisher::publish_pausing`] and
/// [`Publisher::publish_native_pausing`] pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterAt {
    /// After the odd store of word 0.
    Odd,
    /// After the `seg_seq` store.
    SegSeq,
    /// After the `native_seq` store, before the even store of word 0: only a
    /// publication that stores `native_seq` pauses here.
    NativeSeq,
}

/// Where [`ArmReader::read_pausing`] and
/// [`ArmReader::read_publication_pausing`] pause, on each try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderAt {
    /// After the first load of word 0, when its `seq` is even.
    First,
    /// After the `seg_seq` load.
    SegSeq,
    /// After the `native_seq` load, before the fence and the second load.
    NativeSeq,
}

#[cfg(target_has_atomic = "64")]
pub use page::{ArmReader, Publisher};

#[cfg(target_has_atomic = "64")]
mod page {
    use std::sync::atomic::{AtomicU64, Ordering, fence};

    use super::{
        ArmRead, PageError, Publication, READ_RETRIES, ReaderAt, Unread, WORDS_BYTES, WriterAt,
        halves, word,
    };

    /// The three words, at the page's start.
    #[derive(Debug)]
    #[repr(C)]
    struct Words {
        word: AtomicU64,
        seg_seq: AtomicU64,
        native_seq: AtomicU64,
    }

    const _: () = assert!(size_of::<Words>() == WORDS_BYTES);

    /// The words at `ptr`, once `ptr` and `len` are checked.
    ///
    /// # Safety
    ///
    /// When `ptr` is non-null, aligned and `len` covers the words, `ptr` is
    /// valid for reads (and writes, for a writer) of `len` bytes for `'a`,
    /// and every access to the words, in any process, is atomic.
    unsafe fn words<'a>(ptr: *const u8, len: usize) -> Result<&'a Words, PageError> {
        if ptr.is_null() {
            return Err(PageError::Unmapped);
        }
        if !ptr.addr().is_multiple_of(align_of::<Words>()) {
            return Err(PageError::Misaligned);
        }
        if len < WORDS_BYTES {
            return Err(PageError::Short);
        }
        // SAFETY: non-null, aligned and long enough, checked above; valid and
        // accessed atomically, by the caller's contract. `Words` is three
        // `AtomicU64`s, which may be shared while other processes store.
        Ok(unsafe { &*ptr.cast::<Words>() })
    }

    /// The helper's read-only view of the page.
    #[derive(Debug, Clone, Copy)]
    pub struct ArmReader<'a> {
        words: &'a Words,
    }

    impl<'a> ArmReader<'a> {
        /// The page mapped at `ptr`, `len` bytes long. A read-only mapping
        /// is enough: the reader only loads.
        ///
        /// # Safety
        ///
        /// When `ptr` is non-null and 8-byte aligned and `len` is at least
        /// [`WORDS_BYTES`], `ptr` is valid for reads of `len` bytes for
        /// `'a`, and every access to each of the three words (word 0,
        /// `seg_seq` and `native_seq` at offset 16), in any process, is
        /// atomic. Nothing is read through `ptr` otherwise.
        pub unsafe fn at(ptr: *const u8, len: usize) -> Result<Self, PageError> {
            // SAFETY: the caller's contract is `words`'s.
            unsafe { words(ptr, len) }.map(|words| ArmReader { words })
        }

        /// Read the page at `ptr` once: [`ArmReader::at`], then
        /// [`ArmReader::read`], a page that can't be read reading as
        /// [`Unread::Page`].
        ///
        /// # Safety
        ///
        /// As [`ArmReader::at`].
        pub unsafe fn read_at(ptr: *const u8, len: usize) -> ArmRead {
            // SAFETY: the caller's contract is `at`'s.
            match unsafe { ArmReader::at(ptr, len) } {
                Ok(reader) => reader.read(),
                Err(error) => ArmRead::Unread(Unread::Page(error)),
            }
        }

        /// Read `count` and `seg_seq` from one publication.
        pub fn read(&self) -> ArmRead {
            self.read_pausing(|_| {})
        }

        /// [`ArmReader::read`], calling `pause` at each [`ReaderAt`] on each
        /// try: a test's way to hold the reader between its loads. It is
        /// [`ArmReader::read_publication_pausing`] without `native_seq`, so
        /// it pauses at [`ReaderAt::NativeSeq`] too.
        pub fn read_pausing(&self, pause: impl FnMut(ReaderAt)) -> ArmRead {
            self.read_publication_pausing(pause).into()
        }

        /// Read all three words from one publication.
        pub fn read_publication(&self) -> Result<Publication, Unread> {
            self.read_publication_pausing(|_| {})
        }

        /// [`ArmReader::read_publication`], calling `pause` at each
        /// [`ReaderAt`] on each try.
        pub fn read_publication_pausing(
            &self,
            mut pause: impl FnMut(ReaderAt),
        ) -> Result<Publication, Unread> {
            for _ in 0..=READ_RETRIES {
                let first = self.words.word.load(Ordering::Acquire);
                if !halves(first).1.is_multiple_of(2) {
                    continue;
                }
                pause(ReaderAt::First);
                let seg_seq = self.words.seg_seq.load(Ordering::Relaxed);
                pause(ReaderAt::SegSeq);
                let native_seq = self.words.native_seq.load(Ordering::Relaxed);
                pause(ReaderAt::NativeSeq);
                fence(Ordering::Acquire);
                if self.words.word.load(Ordering::Relaxed) == first {
                    return Ok(Publication {
                        count: halves(first).0,
                        seg_seq,
                        native_seq,
                    });
                }
            }
            Err(Unread::Busy)
        }
    }

    /// The daemon's writable view of the page, and the page's only writer.
    #[derive(Debug)]
    pub struct Publisher<'a> {
        words: &'a Words,
        count: u32,
        seq: u32,
    }

    impl<'a> Publisher<'a> {
        /// The page mapped at `ptr`, `len` bytes long, taking up the
        /// publication its words hold (a new, zeroed page holds count 0,
        /// `seq` 0, `seg_seq` 0 and `native_seq` 0).
        ///
        /// # Safety
        ///
        /// As [`ArmReader::at`], with `ptr` also valid for writes; and no
        /// other publisher writes the page while this one lives.
        pub unsafe fn at(ptr: *mut u8, len: usize) -> Result<Self, PageError> {
            // SAFETY: the caller's contract is `words`'s.
            let words = unsafe { words(ptr, len) }?;
            // This is the only writer, so its own last stores are what it
            // reads back.
            let (count, seq) = halves(words.word.load(Ordering::Relaxed));
            Ok(Publisher {
                words,
                count,
                seq: seq.wrapping_add(seq % 2),
            })
        }

        /// Publish `count` and `seg_seq` together, leaving `native_seq` as
        /// the last publication to store it left it.
        pub fn publish(&mut self, count: u32, seg_seq: u64) {
            self.publish_pausing(count, seg_seq, |_| {});
        }

        /// [`Publisher::publish`], calling `pause` at each [`WriterAt`]: a
        /// test's way to park the writer mid-publication.
        pub fn publish_pausing(&mut self, count: u32, seg_seq: u64, pause: impl FnMut(WriterAt)) {
            self.publish_words(count, seg_seq, None, pause);
        }

        /// Publish `count`, `seg_seq` and `native_seq` together.
        pub fn publish_native(&mut self, count: u32, seg_seq: u64, native_seq: u64) {
            self.publish_native_pausing(count, seg_seq, native_seq, |_| {});
        }

        /// [`Publisher::publish_native`], calling `pause` at each
        /// [`WriterAt`].
        pub fn publish_native_pausing(
            &mut self,
            count: u32,
            seg_seq: u64,
            native_seq: u64,
            pause: impl FnMut(WriterAt),
        ) {
            self.publish_words(count, seg_seq, Some(native_seq), pause);
        }

        fn publish_words(
            &mut self,
            count: u32,
            seg_seq: u64,
            native_seq: Option<u64>,
            mut pause: impl FnMut(WriterAt),
        ) {
            let odd = self.seq.wrapping_add(1);
            self.words
                .word
                .store(word(self.count, odd), Ordering::Relaxed);
            fence(Ordering::Release);
            pause(WriterAt::Odd);
            self.words.seg_seq.store(seg_seq, Ordering::Relaxed);
            pause(WriterAt::SegSeq);
            if let Some(native_seq) = native_seq {
                self.words.native_seq.store(native_seq, Ordering::Relaxed);
                pause(WriterAt::NativeSeq);
            }
            self.seq = odd.wrapping_add(1);
            self.count = count;
            self.words
                .word
                .store(word(count, self.seq), Ordering::Release);
        }
    }
}
