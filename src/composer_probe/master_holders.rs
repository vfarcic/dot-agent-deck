//! PRD #1666 part 3 (E-MF, amendment 5 §3.1, amendment 6 §3): whether a
//! process other than the daemon holds a generation's PTY master, read from
//! `/proc` before each drain frontier is published.
//!
//! P-DRN rests on S-M: no process other than the daemon holds the master
//! (A4 §7.1). portable-pty sets `FD_CLOEXEC` on the master only after
//! `openpty` returns, so a fork and exec by another daemon thread in between
//! inherits it. Neither a spawn lock nor an atomic close-on-exec open is
//! available without replacing portable-pty's spawn, so S-M is **checked**:
//! before publishing a frontier, the reader scans every process that could
//! have inherited the master and publishes `Missing` if one holds it, or if
//! one that could can't be read.
//!
//! **Which processes could.** A process gets the master by inheriting it
//! across `fork`, so a holder other than the daemon was started at or after
//! the `openpty` call: either forked from the daemon inside the window, or
//! descended from such a process. The scan reads every process whose start
//! time (`/proc/<pid>/stat`, boot-clock ticks) is at or after the tick the
//! spawn read before calling `openpty`, except the daemon itself and kernel
//! threads, which have no descriptors.
//!
//! **What counts as holding it.** A descriptor whose file is the same `ptmx`
//! inode on the same device as the daemon's master, with the same
//! `tty-index` in its `fdinfo`. A different devpts instance with the same
//! index is the same inode only by coincidence of a bind mount; if it ever
//! matched, the frontier is missing, which holds.
//!
//! **What can't be read.** A process whose descriptor table can't be listed
//! is `Unknown` when its real uid is the daemon's (a set-uid program an
//! inheritor ran keeps that real uid), which makes the frontier missing,
//! unless it has started exiting or is a zombie: the kernel makes an exiting
//! task's descriptor table root's, and such a task runs no more user code.
//! Another user's process can hold the master only by being sent it over a
//! socket, which is outside the threat model, as `ptrace` and
//! `pidfd_getfd(2)` are (A5 §3.1), and is skipped.
//!
//! **What the scan doesn't establish.** It is a reading at one instant:
//! a holder that used the master and closed it, or exited, before the scan
//! isn't seen, and a process forked while `/proc` is listed may be missed
//! with its parent already read. The daemon's own fork-then-exec children
//! hold the master between their fork and their exec; a scan that lands in
//! that window makes the frontier missing, which holds.

use std::io;
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;

/// `PF_KTHREAD` in `/proc/<pid>/stat`'s flags.
const PF_KTHREAD: u64 = 0x0020_0000;
/// `PF_EXITING`: set as `do_exit` starts. Such a task never returns to user
/// space, so it can't use a descriptor it still holds.
const PF_EXITING: u64 = 0x0000_0004;

/// What `/proc/<pid>/stat` says about a process.
#[derive(Debug, Clone, Copy)]
struct Stat {
    /// Its start, in boot-clock ticks.
    start: u64,
    /// A kernel thread: no descriptors.
    kernel: bool,
    /// Exiting or a zombie: it runs no more user code.
    ending: bool,
}

/// A PTY master: its `ptmx` file and its pair's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MasterId {
    dev: u64,
    ino: u64,
    index: u32,
}

/// What a scan found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holders {
    /// No process that could have inherited the master holds it.
    Sole,
    /// This process holds it.
    Other(u32),
    /// This process could hold it and couldn't be read, or `/proc` couldn't
    /// be listed (pid 0): which read failed, and its errno.
    Unknown {
        pid: u32,
        step: &'static str,
        errno: Option<i32>,
    },
}

impl Holders {
    fn unknown(pid: u32, step: &'static str, error: &io::Error) -> Self {
        Self::Unknown {
            pid,
            step,
            errno: error.raw_os_error(),
        }
    }
}

/// The boot clock now, in whole clock ticks: a process started at or after
/// now has a start time at least this.
pub fn boot_ticks() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec to write.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return None;
    }
    // SAFETY: `sysconf` has no memory effects.
    let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    let ns = u64::try_from(ts.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(ts.tv_nsec).ok()?)?;
    Some(u128::from(ns) * u128::from(hz) / 1_000_000_000)
        .and_then(|ticks| u64::try_from(ticks).ok())
}

/// The master open on `fd`, in this process.
pub fn master_id(fd: RawFd) -> io::Result<MasterId> {
    let meta = std::fs::metadata(format!("/proc/self/fd/{fd}"))?;
    let index = tty_index(&format!("/proc/self/fdinfo/{fd}"))?
        .ok_or_else(|| io::Error::other("no tty-index in the master's fdinfo"))?;
    Ok(MasterId {
        dev: meta.dev(),
        ino: meta.ino(),
        index,
    })
}

fn tty_index(fdinfo: &str) -> io::Result<Option<u32>> {
    Ok(std::fs::read_to_string(fdinfo)?
        .lines()
        .find_map(|line| line.strip_prefix("tty-index:"))
        .and_then(|value| value.trim().parse().ok()))
}

/// `pid`'s `/proc/<pid>/stat`.
fn stat(pid: u32) -> io::Result<Stat> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // The command name is parenthesised and may hold anything: the fields
    // after it start at the third.
    let rest = text
        .rfind(')')
        .map(|at| &text[at + 1..])
        .ok_or_else(|| io::Error::other("stat without a command name"))?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let field = |n: usize| {
        fields
            .get(n - 3)
            .copied()
            .ok_or_else(|| io::Error::other(format!("stat field {n}")))
    };
    let number = |n: usize| -> io::Result<u64> {
        field(n)?
            .parse()
            .map_err(|_| io::Error::other(format!("stat field {n}")))
    };
    let flags = number(9)?;
    Ok(Stat {
        start: number(22)?,
        kernel: flags & PF_KTHREAD != 0,
        ending: flags & PF_EXITING != 0 || matches!(field(3)?, "Z" | "X" | "x"),
    })
}

/// `pid`'s real uid, from `/proc/<pid>/status`.
fn real_uid(pid: u32) -> Option<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Whether `pid` holds `master`. `Err` when its descriptors can't be listed.
fn holds(pid: u32, master: &MasterId) -> io::Result<bool> {
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        // Cheap first: only a `ptmx` link can be a master.
        let is_ptmx = std::fs::read_link(&path)
            .is_ok_and(|target| target.as_os_str().as_encoded_bytes().ends_with(b"ptmx"));
        if !is_ptmx {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if (meta.dev(), meta.ino()) != (master.dev, master.ino) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if tty_index(&format!("/proc/{pid}/fdinfo/{name}")).is_ok_and(|i| i == Some(master.index)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether reading a process's `/proc` entry failed because it is gone: no
/// entry since the listing (`ENOENT`), or an exit between the open and the
/// read (`ESRCH`). A process that has gone holds nothing.
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// Where a scan reads processes from: `/proc`, or a test's script.
trait Procs {
    /// Every process's pid.
    fn pids(&self) -> io::Result<Vec<u32>>;
    fn stat(&self, pid: u32) -> io::Result<Stat>;
    /// Whether `pid` holds `master`; `Err` when its descriptors can't be
    /// listed.
    fn holds(&self, pid: u32, master: &MasterId) -> io::Result<bool>;
    fn real_uid(&self, pid: u32) -> Option<u32>;
}

/// The real `/proc`.
struct Proc;

impl Procs for Proc {
    fn pids(&self) -> io::Result<Vec<u32>> {
        Ok(std::fs::read_dir("/proc")?
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect())
    }
    fn stat(&self, pid: u32) -> io::Result<Stat> {
        stat(pid)
    }
    fn holds(&self, pid: u32, master: &MasterId) -> io::Result<bool> {
        holds(pid, master)
    }
    fn real_uid(&self, pid: u32) -> Option<u32> {
        real_uid(pid)
    }
}

/// Scan every process started at or after boot tick `since`, other than this
/// one and kernel threads, for `master`.
pub fn holders(master: &MasterId, since: u64) -> Holders {
    // SAFETY: `getuid` has no memory effects.
    scan(&Proc, master, since, std::process::id(), unsafe {
        libc::getuid()
    })
}

/// [`holders`] over `procs`, for the process `me` of real uid `uid`.
fn scan(procs: &impl Procs, master: &MasterId, since: u64, me: u32, uid: u32) -> Holders {
    let pids = match procs.pids() {
        Ok(pids) => pids,
        Err(e) => return Holders::unknown(0, "proc", &e),
    };
    for pid in pids {
        if pid == me {
            continue;
        }
        let first = match procs.stat(pid) {
            Ok(stat) => stat,
            Err(e) if gone(&e) => continue,
            Err(e) => return Holders::unknown(pid, "stat", &e),
        };
        if first.kernel || first.start < since {
            continue;
        }
        match procs.holds(pid, master) {
            Ok(false) => {}
            Ok(true) => return Holders::Other(pid),
            Err(e) if gone(&e) => {}
            // A task that has started exiting has no `mm`, which makes its
            // `/proc/<pid>/fd` root's (`task_dump_owner`): unreadable, and
            // it runs no more user code. Read after the failure, since the
            // first read can predate the exit.
            Err(_) if procs.stat(pid).map_or(true, |now| now.ending) => {}
            Err(_) if procs.real_uid(pid).is_some_and(|real| real != uid) => {}
            Err(e) => return Holders::unknown(pid, "fd", &e),
        }
    }
    Holders::Sole
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::fd::AsRawFd;

    fn openpty() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        use std::os::fd::FromRawFd;
        let (mut master, mut slave) = (0, 0);
        // SAFETY: two out-pointers to `c_int`s we own, and null for the rest.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "{}", io::Error::last_os_error());
        // SAFETY: both were just returned to us.
        unsafe {
            (
                std::os::fd::OwnedFd::from_raw_fd(master),
                std::os::fd::OwnedFd::from_raw_fd(slave),
            )
        }
    }

    /// The real `/proc`, but only `pids`: other tests' processes, and the
    /// machine's, can't change what a test sees.
    struct Only(Vec<u32>);

    impl Procs for Only {
        fn pids(&self) -> io::Result<Vec<u32>> {
            Ok(self.0.clone())
        }
        fn stat(&self, pid: u32) -> io::Result<Stat> {
            stat(pid)
        }
        fn holds(&self, pid: u32, master: &MasterId) -> io::Result<bool> {
            holds(pid, master)
        }
        fn real_uid(&self, pid: u32) -> Option<u32> {
            real_uid(pid)
        }
    }

    fn real_scan(master: &MasterId, since: u64, pids: &[u32]) -> Holders {
        // SAFETY: `getuid` has no memory effects.
        let uid = unsafe { libc::getuid() };
        scan(&Only(pids.to_vec()), master, since, std::process::id(), uid)
    }

    /// A child that inherits `fd` (no close-on-exec) and sleeps, as an exec
    /// in portable-pty's window would.
    fn inheritor(fd: RawFd) -> std::process::Child {
        // SAFETY: clearing FD_CLOEXEC on a descriptor this test owns.
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        // SAFETY: as above.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        child
    }

    #[test]
    fn a_master_held_only_here_is_sole_and_an_inheritor_is_seen() {
        let since = boot_ticks().unwrap();
        let (master, _slave) = openpty();
        let id = master_id(master.as_raw_fd()).unwrap();
        let me = std::process::id();
        assert_eq!(
            real_scan(&id, since, &[me]),
            Holders::Sole,
            "this process is skipped"
        );
        let mut child = inheritor(master.as_raw_fd());
        let pids = [me, child.id()];
        assert_eq!(real_scan(&id, since, &pids), Holders::Other(child.id()));
        // Another master isn't this one.
        let (other, _other_slave) = openpty();
        let other = master_id(other.as_raw_fd()).unwrap();
        assert_ne!(other, id);
        assert_eq!(real_scan(&other, since, &pids), Holders::Sole);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(real_scan(&id, since, &pids), Holders::Sole);
    }

    #[test]
    fn a_process_started_before_the_master_is_not_scanned() {
        let (master, _slave) = openpty();
        let id = master_id(master.as_raw_fd()).unwrap();
        let mut child = inheritor(master.as_raw_fd());
        // Settle on a later tick, so the child started before `since`.
        let start = boot_ticks().unwrap();
        while boot_ticks().unwrap() <= start + 1 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let since = boot_ticks().unwrap();
        assert_eq!(real_scan(&id, since, &[child.id()]), Holders::Sole);
        assert_eq!(real_scan(&id, 0, &[child.id()]), Holders::Other(child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn stat_reads_this_process_and_a_zombie() {
        let me = stat(std::process::id()).unwrap();
        assert!(!me.kernel && !me.ending);
        assert!(me.start <= boot_ticks().unwrap());
        // An unreaped child that exited is a zombie: ending.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let begun = std::time::Instant::now();
        while !stat(pid).unwrap().ending {
            assert!(begun.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        child.wait().unwrap();
    }

    /// A scripted `/proc`: each pid's stats in the order they're read (the
    /// last repeats), what listing its descriptors gives, and its real uid.
    #[derive(Default)]
    struct Fake {
        pids: Vec<u32>,
        stats: HashMap<u32, std::cell::RefCell<Vec<io::Result<Stat>>>>,
        fds: HashMap<u32, i32>,
        uids: HashMap<u32, u32>,
    }

    const UID: u32 = 1000;
    const ME: u32 = 1;
    const SINCE: u64 = 500;
    const MASTER: MasterId = MasterId {
        dev: 1,
        ino: 2,
        index: 3,
    };

    fn live(start: u64) -> io::Result<Stat> {
        Ok(Stat {
            start,
            kernel: false,
            ending: false,
        })
    }

    fn err(errno: i32) -> io::Result<Stat> {
        Err(io::Error::from_raw_os_error(errno))
    }

    impl Fake {
        /// `pid` with `stats`, its descriptors listing as `fds` (0 holds
        /// nothing, -1 holds the master, else an errno), real uid `uid`.
        fn with(mut self, pid: u32, stats: Vec<io::Result<Stat>>, fds: i32, uid: u32) -> Self {
            self.pids.push(pid);
            self.stats.insert(pid, std::cell::RefCell::new(stats));
            self.fds.insert(pid, fds);
            self.uids.insert(pid, uid);
            self
        }
        fn scan(&self) -> Holders {
            scan(self, &MASTER, SINCE, ME, UID)
        }
    }

    impl Procs for Fake {
        fn pids(&self) -> io::Result<Vec<u32>> {
            Ok(self.pids.clone())
        }
        fn stat(&self, pid: u32) -> io::Result<Stat> {
            let mut stats = self.stats[&pid].borrow_mut();
            if stats.len() > 1 {
                stats.remove(0)
            } else {
                match &stats[0] {
                    Ok(stat) => Ok(*stat),
                    Err(e) => Err(io::Error::from_raw_os_error(e.raw_os_error().unwrap())),
                }
            }
        }
        fn holds(&self, pid: u32, master: &MasterId) -> io::Result<bool> {
            assert_eq!(*master, MASTER);
            match self.fds[&pid] {
                0 => Ok(false),
                -1 => Ok(true),
                errno => Err(io::Error::from_raw_os_error(errno)),
            }
        }
        fn real_uid(&self, pid: u32) -> Option<u32> {
            self.uids.get(&pid).copied()
        }
    }

    #[test]
    fn the_scan_skips_this_process_old_processes_and_kernel_threads() {
        let kernel = Ok(Stat {
            start: SINCE + 9,
            kernel: true,
            ending: false,
        });
        let fake = Fake::default()
            .with(ME, vec![live(SINCE + 1)], -1, UID)
            .with(10, vec![live(SINCE - 1)], -1, UID)
            .with(11, vec![kernel], libc::EACCES, 0)
            .with(12, vec![live(SINCE)], 0, UID);
        assert_eq!(fake.scan(), Holders::Sole);
        let fake = fake.with(13, vec![live(SINCE)], -1, UID);
        assert_eq!(
            fake.scan(),
            Holders::Other(13),
            "a holder started at `since`"
        );
    }

    #[test]
    fn a_process_that_went_away_holds_nothing() {
        let fake = Fake::default()
            .with(20, vec![err(libc::ENOENT)], -1, UID)
            .with(21, vec![err(libc::ESRCH)], -1, UID)
            .with(22, vec![live(SINCE)], libc::ENOENT, UID)
            .with(23, vec![live(SINCE)], libc::ESRCH, UID);
        assert_eq!(fake.scan(), Holders::Sole);
    }

    #[test]
    fn an_unreadable_process_of_this_user_is_unknown_unless_it_is_ending() {
        let ending = Ok(Stat {
            start: SINCE,
            kernel: false,
            ending: true,
        });
        // Exiting by the time its descriptors couldn't be listed.
        let fake = Fake::default().with(30, vec![live(SINCE), ending], libc::EACCES, UID);
        assert_eq!(fake.scan(), Holders::Sole);
        // Another user's: skipped.
        let fake = Fake::default().with(31, vec![live(SINCE)], libc::EACCES, UID + 1);
        assert_eq!(fake.scan(), Holders::Sole);
        // This user's, live (a set-uid program an inheritor ran, or any
        // non-dumpable process started since).
        let fake = Fake::default().with(32, vec![live(SINCE)], libc::EACCES, UID);
        assert_eq!(
            fake.scan(),
            Holders::Unknown {
                pid: 32,
                step: "fd",
                errno: Some(libc::EACCES)
            }
        );
        // A real uid that can't be read is this user's, for this purpose.
        let mut fake = Fake::default().with(33, vec![live(SINCE)], libc::EACCES, UID);
        fake.uids.clear();
        assert!(matches!(fake.scan(), Holders::Unknown { pid: 33, .. }));
        // A stat that fails otherwise is unknown too.
        let fake = Fake::default().with(34, vec![err(libc::EIO)], 0, UID);
        assert_eq!(
            fake.scan(),
            Holders::Unknown {
                pid: 34,
                step: "stat",
                errno: Some(libc::EIO)
            }
        );
    }
}
