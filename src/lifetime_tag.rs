//! Issue #861: finding a test process's descendants after they have left every
//! process group anyone could signal them through.
//!
//! # The hole this closes
//!
//! `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` is read in exactly three places —
//! `daemon.rs`'s backstop task, `wrap::arm_wrap_self_defense` and
//! `wrap::arm_child_group_backstop` — and each of them bounds the process it
//! runs in, then reaches everything *else* by signalling a **process group**:
//! the daemon self-exits and drops its registry, which `killpg`s each pane;
//! `arm_wrap_self_defense` routes a SIGTERM into the wrapper's own reap loop,
//! which `killpg`s the child; and the forked reaper `killpg`s that same
//! group.
//!
//! A `killpg` cannot reach a process that has left the group, and agents put
//! processes outside it as a matter of routine. Measured live on this box:
//! Claude Code's Bash tool runs every shell in a **session of its own**
//! (`pgid == sid == its own pid`, against the `claude` process's own session) —
//! the behaviour `agent_pty::close_agent` already names as "the `setsid`'d
//! sub-shells Claude Code creates internally". Such a process inherits the cap
//! in its environment and is bounded by nothing at all: measured on the shared
//! dev box, pid 2043710 carrying `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS=300`,
//! `PPID 1`, alive **4 days 01:23** — about 1170x its own cap — with its owning
//! test process dead and its temp root already deleted from under it.
//!
//! # Why a tag rather than a walk of the process tree
//!
//! An escapee is a descendant only until its intermediate ancestors die, and
//! they die first: the reported orphan sat at `ppid = 1`. A parent-chain walk
//! performed when the deadline expires therefore looks at a tree that has
//! already come apart, which is exactly the moment the answer is needed. A tag
//! carried in the environment survives that: `execve` passes the caller's
//! environment on by default, so the tag reaches descendants of every shape
//! unless one deliberately rebuilds its own, and `/proc/<pid>/environ` still
//! reports it after the process's whole ancestry is gone.
//!
//! Inheritance is the only route a tag travels, the needle is matched as an
//! exact NUL-delimited environment entry rather than a substring, and no two
//! tags held by *live* processes collide (see [`LifetimeTag::mint`] for what
//! that guarantee is and is not). So a sweep reaches only processes that
//! inherited the tag from the one child it was armed for — not a sibling pane,
//! not a concurrent test run, not a developer's own agents.
//!
//! # What this covers, and what it does not
//!
//! - It is **Linux-only**. Reading another process's environment needs
//!   `/proc/<pid>/environ`; on macOS the equivalent is a `sysctl` with a
//!   different shape and it is not implemented here, so [`signal_tagged`] is a
//!   no-op returning `0` there and the group-signalling half is unchanged. That
//!   matches the platform gating already in this area — `wrap`'s reaper takes a
//!   `close_range(2)` fast path on Linux and a portable `close` loop elsewhere.
//! - It finds a process only while the tag is **in that process's
//!   environment**. A descendant that rebuilds its environment from scratch
//!   loses the tag and is not found. The reported orphan did carry it, which is
//!   how it was diagnosed, and so does every shell an agent spawns.
//! - It is armed **only when the cap is set**, which the test harness does and
//!   production does not. A production deck mints no tag, exports nothing, and
//!   sweeps nothing. Note the qualifier is on the *variable*, not on the build,
//!   exactly as it is for the cap itself: a developer who exports
//!   `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` and then starts an ordinary deck
//!   gets this machinery too, and their agents' descendants will be terminated
//!   at that deadline.
//! - Nothing here survives a `SIGKILL` of the reaper that calls it. That is the
//!   residual `wrap`'s forked reaper already carries and this does not change:
//!   the reaper leaves its parent's process group so a group kill cannot take
//!   it, but a signal aimed at it directly still ends it, and then the deadline
//!   has no holder.
//!
//! # Async-signal-safety
//!
//! [`signal_tagged`] is called from a `fork`ed reaper in a threaded process, so
//! it allocates nothing and calls nothing off POSIX's async-signal-safe list
//! beyond one reentrant-by-design exception: `open`, `read`, `close`, `kill`,
//! the `getdents64` syscall, and glibc's `__errno_location`, which exists
//! precisely so `errno` is per-thread rather than shared. Every buffer
//! is on the stack and every integer is formatted by hand. [`LifetimeTag::mint`]
//! runs in the *parent*, before the fork, where allocation is fine.

/// Environment variable carrying the per-spawn tag.
///
/// Injected into a spawned child's environment next to
/// `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`, and only when that cap is set — see
/// the module docs. Named `_TEST_` for the same reason the cap is: nothing in
/// production sets it or reads it.
pub const DOT_AGENT_DECK_TEST_LIFETIME_TAG: &str = "DOT_AGENT_DECK_TEST_LIFETIME_TAG";

/// Room for the whole `\0NAME=VALUE\0` needle, so it can travel through a
/// `fork` as a plain `Copy` value with no allocation behind it.
///
/// The two boundary NULs are one byte each, the name is 32, the `=` one more,
/// and [`LifetimeTag::mint`] builds a value of three hex integers joined by `-`
/// — at most 16 + 1 + 8 + 1 + 16 = 42 bytes even at `u64::MAX`. That is 77, so
/// 96 leaves the name room to grow. `mint` and [`LifetimeTag::inherited`] refuse
/// rather than truncate if it ever does not fit, because a truncated needle is a
/// *prefix*, and a prefix is exactly what the boundaries exist to stop matching.
const NEEDLE_CAP: usize = 96;

/// A tag unique to one spawn: the `NAME=VALUE` bytes to put in the child's
/// environment, and — wrapped in the NUL boundaries that make it an exact
/// environment *entry* rather than a substring — the needle its reaper searches
/// for.
///
/// `Copy` and self-contained on purpose — it is captured before a `fork` and
/// read in the child, where dereferencing anything the parent allocated would
/// be a bug waiting for the allocator lock to be held at the wrong moment.
#[derive(Clone, Copy)]
pub struct LifetimeTag {
    needle: [u8; NEEDLE_CAP],
    len: usize,
}

impl LifetimeTag {
    /// Mint a tag for one spawn, or `None` if the needle would not fit.
    ///
    /// # What the uniqueness guarantee actually is
    ///
    /// **No two tags held by processes alive at the same time collide**, and
    /// that — not global uniqueness — is what a sweep needs, because a sweep can
    /// only ever signal a live process. Three parts, each closing a hole the
    /// others leave:
    ///
    /// - the spawning process's **pid** separates concurrent decks, daemons and
    ///   test binaries;
    /// - a per-process **counter** separates panes spawned by one of them;
    /// - `CLOCK_MONOTONIC` **nanoseconds** separate a pid from the same pid
    ///   after reuse, which is the case the first two cannot see.
    ///
    /// State the residual rather than claiming more. `CLOCK_MONOTONIC` restarts
    /// at boot and the counter restarts per process, so two tags minted on
    /// *different boots* can collide. That is inert: a process cannot outlive
    /// the boot it was tagged on, so a colliding tag can never name a live
    /// process, and a reaper is itself gone with its boot. Within one boot the
    /// guarantee holds — a pid is reusable, but a reused pid means the earlier
    /// holder is dead, and the nanosecond component distinguishes the two.
    ///
    /// [`minted_tags_are_unique`] measures the within-process half over 1000
    /// mints; the cross-process half rests on the pid, and the cross-reuse half
    /// on the clock.
    ///
    /// Allocates. Call it in the parent, before any `fork`.
    ///
    /// [`minted_tags_are_unique`]: self
    pub fn mint() -> Option<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        Self::from_value(&format!(
            "{:x}-{:x}-{:x}",
            std::process::id(),
            seq,
            monotonic_nanos()
        ))
    }

    /// Build the boundary-wrapped needle for `value`, or `None` if it would not
    /// fit in [`NEEDLE_CAP`].
    ///
    /// The needle is `\0NAME=VALUE\0`, not `NAME=VALUE`. An environment block is
    /// NUL-separated, so the two boundaries are what make a match an exact
    /// *entry*: without them the needle is a bare substring, and one tag that
    /// happens to be a prefix of another — or the same text sitting inside some
    /// other variable's value — matches too. That matters more here than it
    /// usually would, because a match is followed by `SIGTERM` and then
    /// `SIGKILL`, so an overbroad match is a signal delivered to a process this
    /// spawn has no claim on.
    fn from_value(value: &str) -> Option<Self> {
        let text = format!("\0{DOT_AGENT_DECK_TEST_LIFETIME_TAG}={value}\0");
        let bytes = text.as_bytes();
        if bytes.len() > NEEDLE_CAP {
            return None;
        }
        let mut needle = [0u8; NEEDLE_CAP];
        needle[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            needle,
            len: bytes.len(),
        })
    }

    /// The tag to give a child of this process, or `None` when this mechanism
    /// is not armed at all.
    ///
    /// Two rules, and the second is the one worth reading.
    ///
    /// **Armed only when `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` names a usable
    /// cap.** The harness sets that variable and production does not, so a
    /// production deck mints nothing, exports nothing, and sweeps nothing. It is
    /// the same gate `wrap::arm_wrap_self_defense` and
    /// `wrap::arm_child_group_backstop` already use, read here so the rule lives
    /// in one place rather than one copy per spawn site.
    ///
    /// **A tag this process already carries is REUSED rather than replaced**, so
    /// one tag covers a whole spawned subtree and any reaper armed anywhere in it
    /// can find any escapee from it. The alternative was measured against and is
    /// worse: minting at every level makes each level's tag *overwrite* its
    /// parent's in the child's environment, because `Command::env` replaces
    /// rather than appends. A pane's own reaper would then be unable to see past
    /// its `wrap`, and coverage would rest on the innermost reaper being the one
    /// that survives — the exact single-point-of-failure assumption issue #861 is
    /// about. Per-*pane* isolation is unaffected: `agent_pty::spawn` in a daemon
    /// carries no tag of its own, so each pane still mints a distinct one and no
    /// pane's reaper can reach another's descendants.
    pub fn for_child() -> Option<Self> {
        std::env::var(crate::agent_pty::DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS)
            .ok()
            .and_then(|value| crate::daemon::parse_max_lifetime_secs(&value))?;
        Self::inherited().or_else(Self::mint)
    }

    /// The tag in *this* process's own environment, when it has one.
    ///
    /// `None` for an absent, empty, or over-long value — each of which would
    /// otherwise produce a needle that matches either nothing or the wrong
    /// thing, and "the wrong thing" is a signal sent to a process this spawn has
    /// no claim on.
    pub fn inherited() -> Option<Self> {
        let value = std::env::var(DOT_AGENT_DECK_TEST_LIFETIME_TAG).ok()?;
        // An empty value would build the needle `\0NAME=\0`, which matches any
        // process that carries the variable empty — including ones from another
        // run. A NUL in the value cannot come from `execve` and would truncate
        // the needle's own boundary.
        if value.is_empty() || value.contains('\0') {
            return None;
        }
        Self::from_value(&value)
    }

    /// The value to set [`DOT_AGENT_DECK_TEST_LIFETIME_TAG`] to on the child.
    ///
    /// The needle it is carved out of is `\0NAME=VALUE\0`, so this drops the
    /// trailing boundary as well as everything up to the `=`.
    ///
    /// Infallible: the needle is built by [`Self::from_value`] from ASCII, and
    /// both the `=` and the trailing NUL are always present.
    pub fn value(&self) -> &str {
        let needle = self.needle();
        let split = needle
            .iter()
            .position(|b| *b == b'=')
            .expect("a built needle always contains its `=`");
        let end = needle.len() - 1;
        std::str::from_utf8(&needle[split + 1..end]).expect("a built needle is ASCII")
    }

    /// The `\0NAME=VALUE\0` bytes to search a `/proc/<pid>/environ` for — an
    /// exact entry, boundaries included. See [`Self::from_value`].
    pub fn needle(&self) -> &[u8] {
        &self.needle[..self.len]
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds, for [`LifetimeTag::mint`]'s uniqueness
/// component. Monotonic rather than wall-clock so a `settimeofday` cannot hand
/// two spawns the same value.
#[cfg(unix)]
fn monotonic_nanos() -> u64 {
    // SAFETY: `clock_gettime` fills the `timespec` it is handed and touches
    // nothing else.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

#[cfg(not(unix))]
fn monotonic_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Send `signal` to every process whose environment carries `needle`, and
/// return how many matched.
///
/// `signal` may be `0`, which delivers nothing and makes this a pure count —
/// how the reaper asks "is anything still out there?" without disturbing it.
///
/// `self_pid` is skipped so a caller cannot signal itself. It is belt and
/// braces rather than the argument: a reaper is a `fork` of the *spawning*
/// process, and the tag is injected into the **child's** environment only, so a
/// reaper never carries the needle it hunts.
///
/// Errors are swallowed by design. A `/proc` entry that vanishes mid-sweep, an
/// `environ` owned by another user (`EACCES`), a pid reaped between the read and
/// the signal (`ESRCH`) — all of them mean "not our business" and all of them
/// are the common case on a shared box.
///
/// # Safety
///
/// Async-signal-safe: no allocation, and no libc call outside POSIX's list. Safe
/// to call from a `fork`ed child of a threaded process.
#[cfg(target_os = "linux")]
pub unsafe fn signal_tagged(needle: &[u8], signal: libc::c_int, self_pid: libc::pid_t) -> usize {
    /// One `getdents64` batch. 8-aligned because the kernel packs `dirent64`
    /// records at that alignment.
    #[repr(C, align(8))]
    struct Batch([u8; 4096]);

    /// Where a record's NUL-terminated name starts, and where its 16-bit length
    /// sits, as byte offsets into the record.
    ///
    /// Records are walked through these rather than through a
    /// `*const libc::dirent64` place expression, and that is a correctness point
    /// rather than a style one: the kernel sizes each record to its actual name
    /// length, so the struct's 256-byte `d_name` array reaches past the end of
    /// the record — often past the end of the buffer — even though the
    /// NUL-terminated name inside it does not. Byte offsets keep every access
    /// inside `batch`.
    const NAME_AT: usize = std::mem::offset_of!(libc::dirent64, d_name);
    const RECLEN_AT: usize = std::mem::offset_of!(libc::dirent64, d_reclen);

    if needle.is_empty() {
        return 0;
    }
    const PROC: &[u8] = b"/proc\0";
    // SAFETY: a NUL-terminated literal; `open` reads it and nothing else.
    let dir = unsafe {
        libc::open(
            PROC.as_ptr() as *const libc::c_char,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
    };
    if dir < 0 {
        return 0;
    }

    let mut matched = 0usize;
    let mut batch = Batch([0u8; 4096]);
    loop {
        // SAFETY: `getdents64` fills at most `len` bytes of the buffer it is
        // handed. The raw syscall rather than a libc wrapper because glibc
        // exposes none that is guaranteed allocation-free.
        let read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                dir,
                batch.0.as_mut_ptr(),
                batch.0.len(),
            )
        };
        if read <= 0 {
            break;
        }
        let filled = read as usize;
        let mut offset = 0usize;
        while offset + NAME_AT <= filled {
            // SAFETY: the bound above puts the whole record header inside the
            // buffer. Read unaligned so the walk does not also depend on where
            // the kernel chose to place this record.
            let reclen = unsafe {
                batch
                    .0
                    .as_ptr()
                    .add(offset + RECLEN_AT)
                    .cast::<u16>()
                    .read_unaligned()
            } as usize;
            // A record shorter than its own header, or one running past what
            // was filled, means the walk has lost sync — and `<= NAME_AT` also
            // guarantees the loop makes progress.
            if reclen <= NAME_AT || offset + reclen > filled {
                break;
            }
            // SAFETY: the name is NUL-terminated within this record, which
            // `offset + reclen <= filled` places inside the buffer.
            let name = unsafe { batch.0.as_ptr().add(offset + NAME_AT) }.cast::<libc::c_char>();
            offset += reclen;
            if let Some(pid) = unsafe { parse_pid(name) }
                && pid != self_pid
                && unsafe { environ_contains(pid, needle) }
            {
                matched += 1;
                // SAFETY: `kill` is async-signal-safe; a pid that has since
                // been reaped simply returns `ESRCH`, which we ignore.
                unsafe { libc::kill(pid, signal) };
            }
        }
    }

    // SAFETY: `dir` is this function's own descriptor.
    unsafe { libc::close(dir) };
    matched
}

/// Non-Linux stub: there is no `/proc/<pid>/environ` to read, so no escapee can
/// be found and the caller falls back to signalling the process group alone —
/// exactly what it did before this existed. See the module docs.
///
/// # Safety
///
/// None required: this arm signals nothing and reads nothing. `unsafe` is kept
/// only so the signature matches the Linux one, where it is load-bearing — the
/// caller is a `fork`ed reaper and must not be given a differently-shaped
/// function to call depending on the target.
#[cfg(all(unix, not(target_os = "linux")))]
pub unsafe fn signal_tagged(_needle: &[u8], _signal: libc::c_int, _self_pid: libc::pid_t) -> usize {
    0
}

/// A `/proc` entry name as a pid, or `None` when it is not one (`self`, `net`,
/// `1234abc`).
///
/// # Safety
///
/// `name` must point at a NUL-terminated byte string.
#[cfg(target_os = "linux")]
unsafe fn parse_pid(name: *const libc::c_char) -> Option<libc::pid_t> {
    let mut value: libc::pid_t = 0;
    let mut digits = 0usize;
    loop {
        // SAFETY: the caller guarantees a NUL terminator, which ends the loop.
        let byte = unsafe { *name.add(digits) } as u8;
        if byte == 0 {
            break;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        // A pid cannot exceed 2^22 on Linux, so 7 digits is the whole space and
        // an 8th means this is not a pid directory. Bailing out beats letting
        // the accumulator saturate into a plausible-looking pid.
        digits += 1;
        if digits > 7 {
            return None;
        }
        value = value * 10 + libc::pid_t::from(byte - b'0');
    }
    // `0` is not a pid, and `1` is `init` — never ours, and the one pid where a
    // stray signal would be worst.
    (digits > 0 && value > 1).then_some(value)
}

/// Whether `/proc/<pid>/environ` contains `needle`.
///
/// Streams the file through a fixed window, carrying `needle.len() - 1` bytes
/// across each read so a match straddling two reads is still found. An
/// environment can be tens of kilobytes (`ARG_MAX`), and the tag sits wherever
/// the spawner happened to put it, so reading only the first block would find
/// it by luck rather than by construction.
///
/// `needle` is a whole entry with its own NUL boundaries (`\0NAME=VALUE\0`), and
/// the **first** entry in the block has no NUL in front of it — so the stream is
/// primed with one synthetic NUL, which makes every entry uniformly preceded by
/// a boundary and costs nothing. The trailing boundary comes from the block's
/// own separators; Linux terminates the block with a NUL, so the last entry has
/// one too. A process that has rewritten its own environ into something without
/// a trailing NUL is the one shape this can miss, which is a benign false
/// negative — it declines to signal rather than signalling the wrong thing.
///
/// # Safety
///
/// Async-signal-safe: a stack buffer, `open`/`read`/`close`, and a byte compare.
#[cfg(target_os = "linux")]
unsafe fn environ_contains(pid: libc::pid_t, needle: &[u8]) -> bool {
    let mut path = [0u8; 32];
    if !write_environ_path(pid, &mut path) {
        return false;
    }
    // SAFETY: `path` was just NUL-terminated by `write_environ_path`.
    let fd = unsafe { libc::open(path.as_ptr() as *const libc::c_char, libc::O_RDONLY) };
    if fd < 0 {
        return false;
    }

    let mut window = [0u8; 4096 + NEEDLE_CAP];
    let carry = needle.len() - 1;
    // The synthetic leading boundary described above: reads append after it.
    window[0] = 0;
    let mut held = 1usize;
    let mut found = false;
    loop {
        let want = window.len() - held;
        if want == 0 {
            break;
        }
        // SAFETY: writes at most `want` bytes at `held`, both inside `window`.
        let read =
            unsafe { libc::read(fd, window.as_mut_ptr().add(held) as *mut libc::c_void, want) };
        if read < 0 {
            // SAFETY: `__errno_location` returns this thread's errno slot.
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            break;
        }
        if read == 0 {
            break;
        }
        let filled = held + read as usize;
        if contains(&window[..filled], needle) {
            found = true;
            break;
        }
        if filled > carry {
            window.copy_within(filled - carry..filled, 0);
            held = carry;
        } else {
            held = filled;
        }
    }

    // SAFETY: `fd` is this function's own descriptor.
    unsafe { libc::close(fd) };
    found
}

/// Write `/proc/<pid>/environ\0` into `buf`, or return `false` if it does not
/// fit.
///
/// Hand-formatted because `format!` allocates and this runs after a `fork`.
#[cfg(target_os = "linux")]
fn write_environ_path(pid: libc::pid_t, buf: &mut [u8; 32]) -> bool {
    const PREFIX: &[u8] = b"/proc/";
    const SUFFIX: &[u8] = b"/environ\0";

    if pid <= 0 {
        return false;
    }
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    let mut rest = pid as u64;
    while rest > 0 {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
    }
    if PREFIX.len() + count + SUFFIX.len() > buf.len() {
        return false;
    }
    let mut at = 0usize;
    buf[at..at + PREFIX.len()].copy_from_slice(PREFIX);
    at += PREFIX.len();
    for i in 0..count {
        buf[at] = digits[count - 1 - i];
        at += 1;
    }
    buf[at..at + SUFFIX.len()].copy_from_slice(SUFFIX);
    true
}

/// Naive substring search over bytes.
///
/// Naive on purpose: it allocates nothing and calls nothing, which
/// `memmem(3)` — a GNU extension that is not on POSIX's async-signal-safe list
/// — cannot promise. The needle is under 96 bytes and the haystack is one 4 KiB
/// window, so the worst case is a few hundred thousand byte compares per
/// process, paid at most twice per reaper.
#[cfg(target_os = "linux")]
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_minted_tag_round_trips_through_its_needle() {
        let tag = LifetimeTag::mint().expect("mint a tag");
        let needle = tag.needle();
        assert_eq!(
            needle.first().copied(),
            Some(0),
            "the needle must open with a NUL boundary, or it can match a \
             substring of some other entry rather than an entry of its own"
        );
        assert_eq!(
            needle.last().copied(),
            Some(0),
            "the needle must close with a NUL boundary, or a tag that is a \
             PREFIX of another tag matches that tag's entry"
        );
        assert_eq!(
            std::str::from_utf8(needle).unwrap(),
            format!("\0{DOT_AGENT_DECK_TEST_LIFETIME_TAG}={}\0", tag.value()),
            "`value()` must be exactly the span between the `=` and the closing \
             boundary"
        );
        assert!(
            !tag.value().is_empty(),
            "an empty value would match nothing"
        );
    }

    /// Scenario: build two tags where one value strictly extends the other, plus
    /// an environment block with the tag's text buried inside an unrelated
    /// variable's value, and assert no needle matches anything but its own
    /// entry.
    ///
    /// The regression guard for a review finding on this PR. The needle was a
    /// bare `NAME=VALUE`, so the shorter tag matched the longer one's entry as a
    /// substring — and since every match receives SIGTERM and then SIGKILL, an
    /// overbroad match is a signal delivered to a process the sweep has no
    /// claim on.
    #[test]
    fn a_tag_that_is_a_prefix_of_another_does_not_match_it() {
        let short = LifetimeTag::from_value("abc").expect("build the shorter tag");
        let long = LifetimeTag::from_value("abcd").expect("build the longer tag");

        // How each appears in a real environment block: NUL-separated entries.
        let short_env = format!("A=1\0{DOT_AGENT_DECK_TEST_LIFETIME_TAG}=abc\0B=2\0");
        let long_env = format!("A=1\0{DOT_AGENT_DECK_TEST_LIFETIME_TAG}=abcd\0B=2\0");

        assert!(
            contains(short_env.as_bytes(), short.needle()),
            "a tag must still match its own entry"
        );
        assert!(
            contains(long_env.as_bytes(), long.needle()),
            "a tag must still match its own entry"
        );
        assert!(
            !contains(long_env.as_bytes(), short.needle()),
            "the shorter tag matched the longer tag's entry — the sweep would \
             signal another spawn's descendants"
        );
        assert!(
            !contains(short_env.as_bytes(), long.needle()),
            "the longer tag matched the shorter tag's entry"
        );
        let embedded = format!("A=1\0OTHER={DOT_AGENT_DECK_TEST_LIFETIME_TAG}=abc\0B=2\0");
        assert!(
            !contains(embedded.as_bytes(), short.needle()),
            "the tag matched inside another variable's value, which is not an \
             entry of its own"
        );
    }

    #[test]
    fn minted_tags_are_unique() {
        // The per-process counter is what makes this hold even when two mints
        // land in the same clock tick, and uniqueness is the whole safety
        // argument for sweeping by tag.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(LifetimeTag::mint().expect("mint a tag").value().to_string()),
                "two spawns minted the same tag, so one spawn's reaper could \
                 signal the other's descendants"
            );
        }
    }

    #[test]
    fn an_inherited_tag_is_parsed_back_into_the_same_needle() {
        let minted = LifetimeTag::mint().expect("mint a tag");
        // `for_child` reuses whatever this process carries, so the needle a
        // parent's reaper searches for and the one a nested `wrap`'s reaper
        // searches for must be byte-identical — otherwise neither can see the
        // other's escapees, which is the single-point-of-failure this reuse
        // exists to avoid. `inherited()` rebuilds through the same
        // `from_value`, which is what makes that hold — and going through the
        // value is the round trip an environment variable actually takes.
        let reparsed = LifetimeTag::from_value(minted.value()).expect("rebuild from the value");
        assert_eq!(reparsed.needle(), minted.needle());
        assert_eq!(reparsed.value(), minted.value());

        // A value too long for the needle must be REFUSED, not truncated: a
        // truncated needle is a prefix, and matching a prefix is exactly what
        // the boundaries exist to prevent.
        assert!(
            LifetimeTag::from_value(&"x".repeat(NEEDLE_CAP)).is_none(),
            "an over-long value must be refused rather than truncated"
        );
    }

    #[test]
    fn environ_paths_are_formatted_by_hand_correctly() {
        let mut buf = [0u8; 32];
        assert!(write_environ_path(1234, &mut buf));
        assert_eq!(
            std::ffi::CStr::from_bytes_until_nul(&buf).unwrap().to_str(),
            Ok("/proc/1234/environ")
        );

        let mut buf = [0u8; 32];
        assert!(write_environ_path(4_194_304, &mut buf));
        assert_eq!(
            std::ffi::CStr::from_bytes_until_nul(&buf).unwrap().to_str(),
            Ok("/proc/4194304/environ")
        );

        // Zero and negative are not pids; refusing beats formatting `/proc/0/`.
        assert!(!write_environ_path(0, &mut [0u8; 32]));
        assert!(!write_environ_path(-1, &mut [0u8; 32]));
    }

    #[test]
    fn pid_directory_names_are_told_apart_from_the_rest_of_proc() {
        let parse = |name: &str| {
            let c = std::ffi::CString::new(name).unwrap();
            // SAFETY: `c` is NUL-terminated and outlives the call.
            unsafe { parse_pid(c.as_ptr()) }
        };
        assert_eq!(parse("1234"), Some(1234));
        assert_eq!(parse("4194304"), Some(4_194_304));
        // Not pids: `/proc` is full of these and each one would otherwise cost
        // an `open` of a path that cannot exist.
        assert_eq!(parse("self"), None);
        assert_eq!(parse("thread-self"), None);
        assert_eq!(parse("net"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("12x"), None);
        // `0` is not a pid and `1` is `init` — the one pid where a stray signal
        // would be worst, so it is excluded by construction rather than by the
        // caller remembering to.
        assert_eq!(parse("0"), None);
        assert_eq!(parse("1"), None);
        // Wider than the pid space, so an accumulator cannot saturate into a
        // plausible-looking pid.
        assert_eq!(parse("99999999"), None);
    }

    #[test]
    fn the_substring_search_finds_matches_anywhere_including_the_edges() {
        assert!(contains(b"abcdef", b"abc"));
        assert!(contains(b"abcdef", b"def"));
        assert!(contains(b"abcdef", b"cd"));
        assert!(contains(b"abc", b"abc"));
        assert!(!contains(b"abcdef", b"cf"));
        assert!(!contains(b"ab", b"abc"));
        assert!(!contains(b"abc", b""));
        // NUL-separated, like a real `environ`.
        assert!(contains(b"A=1\0TAG=xyz\0B=2\0", b"TAG=xyz"));
        assert!(!contains(b"A=1\0TAG=xy\0B=2\0", b"TAG=xyz"));
    }

    /// Scenario: run a real child that carries a minted tag in its environment
    /// and a control child that does not, then sweep with signal `0` and assert
    /// the tagged one is counted and the untagged one is not.
    ///
    /// The sweep is what does the killing, so "does it find the right
    /// processes?" is the property worth a real process rather than a fixture.
    /// Signal `0` delivers nothing, so this counts without disturbing anything.
    #[test]
    fn the_sweep_finds_a_tagged_child_and_leaves_an_untagged_one_alone() {
        use std::process::{Command, Stdio};

        let tag = LifetimeTag::mint().expect("mint a tag");
        let mut tagged = Command::new("/bin/sleep")
            .arg("30")
            .env(DOT_AGENT_DECK_TEST_LIFETIME_TAG, tag.value())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the tagged child");
        let mut untagged = Command::new("/bin/sleep")
            .arg("30")
            .env_remove(DOT_AGENT_DECK_TEST_LIFETIME_TAG)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the untagged child");
        // A decoy whose tag value strictly EXTENDS ours. With the needle's NUL
        // boundaries this must not match; without them it did, and the sweep
        // would have SIGKILLed a process belonging to no spawn of ours.
        let mut decoy = Command::new("/bin/sleep")
            .arg("30")
            .env(
                DOT_AGENT_DECK_TEST_LIFETIME_TAG,
                format!("{}extra", tag.value()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the prefix-collision decoy");

        // `execve` is asynchronous with respect to `spawn` returning, and
        // `/proc/<pid>/environ` reports the *exec'd* image's environment, so
        // poll rather than assuming the child is already there.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut matched = 0;
        while std::time::Instant::now() < deadline {
            // SAFETY: signal 0 delivers nothing; this is a pure count.
            matched = unsafe { signal_tagged(tag.needle(), 0, libc::getpid()) };
            if matched > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let other = LifetimeTag::mint().expect("mint a second tag");
        // SAFETY: signal 0 delivers nothing.
        let strangers = unsafe { signal_tagged(other.needle(), 0, libc::getpid()) };

        let _ = tagged.kill();
        let _ = tagged.wait();
        let _ = untagged.kill();
        let _ = untagged.wait();
        let _ = decoy.kill();
        let _ = decoy.wait();

        assert_eq!(
            matched, 1,
            "the sweep must find exactly the one child carrying this tag — \
             finding none means an escapee would never be reached, and finding \
             more means it reaches processes that are not this spawn's, which \
             here would be the decoy whose value merely EXTENDS ours"
        );
        assert_eq!(
            strangers, 0,
            "a freshly minted tag must match nothing, or the sweep is keying on \
             something other than the per-spawn value and could signal another \
             run's processes"
        );
    }
}
