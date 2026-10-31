//! PRD #1666 part 3 (E-MF, amendment 5 §3.1, amendment 6 §3): a census of
//! which other processes hold a generation's PTY master, read from `/proc`
//! before each drain frontier is published.
//!
//! **What this is not.** P-DRN rests on S-M: no process other than the daemon
//! holds the master (A4 §7.1). This census neither establishes nor enforces
//! S-M, and **E-MF stays UNVERIFIED** (Q86): it is one reading after the zero
//! poll, so a holder that used the master and closed it, or exited, before the
//! reading isn't seen, a process forked after its pid listing isn't read, and
//! a master-side change that outlives its process isn't visible. What it adds
//! is that a frontier is published `Missing` whenever the reading finds a
//! holder or can't be completed, so the observation code under `Hold` never
//! takes a frontier over evidence it saw or couldn't read.
//!
//! **Why the window exists.** portable-pty sets `FD_CLOEXEC` on the master
//! only after `openpty` returns, so a fork and exec by another daemon thread
//! in between inherits it. A causal fix (an atomic close-on-exec pair from a
//! portable-pty patch or a pair-creation broker, with a fork-without-exec
//! analysis) isn't part-3 work (Q86).
//!
//! **Which processes are read.** Inheritance across `fork` is the route this
//! census covers; passing the master over a socket, `ptrace` and
//! `pidfd_getfd(2)` are outside the threat model (A5 §3.1). A process that
//! inherited the master started at or after the `openpty` call, so the census
//! reads the processes whose start (`/proc/<pid>/stat`, boot-clock ticks) is
//! at or after the tick the spawn read before calling `openpty`, except the
//! daemon itself and kernel threads.
//!
//! **Per thread.** Within such a process, it reads each task's descriptor
//! table (`/proc/<pid>/task/<tid>/fd`): a thread may have unshared its table,
//! and a leader that exited with `pthread_exit` reads as a zombie while a
//! sibling still runs with the shared table. A task that has started
//! exiting, or is a zombie, runs no more user code; its own unreadable table
//! is skipped, and a table it shares is read through the siblings that use
//! it.
//!
//! **What counts as holding it.** A descriptor whose file is the same `ptmx`
//! inode on the same device as the daemon's master, with the same
//! `tty-index` in its `fdinfo`. A `ptmx` descriptor of that inode with no
//! readable index counts as unknown.
//!
//! **Unknown, not "none".** Every read that fails other than by the process,
//! task or descriptor having gone (`ENOENT`, `ESRCH`) makes the census
//! `Unknown`, with one exception: a task whose descriptor table can't be
//! listed is skipped when it is exiting or a zombie (re-read after the
//! failure), or when its real uid isn't the daemon's (another user's process
//! gets the master only by a route outside the threat model; a set-uid
//! program an inheritor ran keeps the daemon's real uid). The cost: any
//! same-user process that can't be read and started after an agent's spawn,
//! such as an `sshd-session` per SSH login, makes that agent's frontiers
//! `Missing` while it lives (Q85/Q86 keep it so).

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
    /// The census found no holder among the tasks it read. Not S-M: see
    /// the module documentation.
    NoneFound,
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
    Ok(parse_tty_index(&std::fs::read_to_string(fdinfo)?))
}

/// A task: a thread `tid` of process `pid` (the leader is `tid == pid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Task {
    pid: u32,
    tid: u32,
}

/// Parse `/proc/.../stat`.
fn parse_stat(text: &str) -> io::Result<Stat> {
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

/// A real uid from a `status` file's text.
fn parse_real_uid(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// A `fdinfo` file's `tty-index`, `None` when it has none.
fn parse_tty_index(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.strip_prefix("tty-index:"))
        .and_then(|value| value.trim().parse().ok())
}

/// The names in a directory, every entry error kept.
fn names(dir: &str) -> io::Result<Vec<String>> {
    entry_names(std::fs::read_dir(dir)?.map(|entry| entry.map(|e| e.file_name())))
}

/// [`names`]'s collector: the first entry error is the result, even after
/// entries that were read.
fn entry_names(
    entries: impl Iterator<Item = io::Result<std::ffi::OsString>>,
) -> io::Result<Vec<String>> {
    entries
        .map(|name| Ok(name?.to_string_lossy().into_owned()))
        .collect()
}

/// The reads the census makes, one file at a time: `/proc`, or a test's
/// script.
trait Procs {
    /// Every process's pid.
    fn pids(&self) -> io::Result<Vec<u32>>;
    /// A process's tasks.
    fn tasks(&self, pid: u32) -> io::Result<Vec<u32>>;
    fn stat(&self, task: Task) -> io::Result<Stat>;
    fn real_uid(&self, task: Task) -> io::Result<Option<u32>>;
    /// A task's descriptor names.
    fn fds(&self, task: Task) -> io::Result<Vec<String>>;
    /// A descriptor's link text.
    fn link(&self, task: Task, fd: &str) -> io::Result<Vec<u8>>;
    /// A descriptor's file: its device and inode.
    fn file(&self, task: Task, fd: &str) -> io::Result<(u64, u64)>;
    /// A descriptor's `tty-index`.
    fn index(&self, task: Task, fd: &str) -> io::Result<Option<u32>>;
}

/// The real `/proc`.
struct Proc;

impl Proc {
    fn base(task: Task) -> String {
        format!("/proc/{}/task/{}", task.pid, task.tid)
    }
}

impl Procs for Proc {
    fn pids(&self) -> io::Result<Vec<u32>> {
        Ok(names("/proc")?
            .iter()
            .filter_map(|name| name.parse().ok())
            .collect())
    }
    fn tasks(&self, pid: u32) -> io::Result<Vec<u32>> {
        names(&format!("/proc/{pid}/task"))?
            .iter()
            .map(|name| {
                name.parse()
                    .map_err(|_| io::Error::other(format!("task name {name}")))
            })
            .collect()
    }
    fn stat(&self, task: Task) -> io::Result<Stat> {
        parse_stat(&std::fs::read_to_string(format!(
            "{}/stat",
            Self::base(task)
        ))?)
    }
    fn real_uid(&self, task: Task) -> io::Result<Option<u32>> {
        Ok(parse_real_uid(&std::fs::read_to_string(format!(
            "{}/status",
            Self::base(task)
        ))?))
    }
    fn fds(&self, task: Task) -> io::Result<Vec<String>> {
        names(&format!("{}/fd", Self::base(task)))
    }
    fn link(&self, task: Task, fd: &str) -> io::Result<Vec<u8>> {
        Ok(std::fs::read_link(format!("{}/fd/{fd}", Self::base(task)))?
            .into_os_string()
            .into_encoded_bytes())
    }
    fn file(&self, task: Task, fd: &str) -> io::Result<(u64, u64)> {
        let meta = std::fs::metadata(format!("{}/fd/{fd}", Self::base(task)))?;
        Ok((meta.dev(), meta.ino()))
    }
    fn index(&self, task: Task, fd: &str) -> io::Result<Option<u32>> {
        Ok(parse_tty_index(&std::fs::read_to_string(format!(
            "{}/fdinfo/{fd}",
            Self::base(task)
        ))?))
    }
}

/// Whether reading a process's, task's or descriptor's `/proc` entry failed
/// because it is gone: no entry since the listing (`ENOENT`), or an exit
/// between the open and the read (`ESRCH`). What has gone holds nothing.
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// Whether `task`'s table holds `master`. A descriptor that went away is
/// skipped; any other failed read is `Err`, and so is a `ptmx` descriptor of
/// this inode with no `tty-index`.
fn holds(procs: &impl Procs, task: Task, master: &MasterId) -> io::Result<bool> {
    for fd in procs.fds(task)? {
        let target = match procs.link(task, &fd) {
            Ok(target) => target,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e),
        };
        // Cheap first: only a `ptmx` link can be a master.
        if !target.ends_with(b"ptmx") {
            continue;
        }
        match procs.file(task, &fd) {
            Ok(file) if file == (master.dev, master.ino) => {}
            Ok(_) => continue,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e),
        }
        match procs.index(task, &fd) {
            Ok(Some(index)) if index == master.index => return Ok(true),
            Ok(Some(_)) => {}
            Ok(None) => return Err(io::Error::other("a ptmx descriptor without tty-index")),
            Err(e) if gone(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// Read every task of every process started at or after boot tick `since`,
/// other than this process and kernel threads, for `master`. See the module
/// documentation for what the result does and doesn't say.
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
        let leader = Task { pid, tid: pid };
        match procs.stat(leader) {
            Ok(stat) if stat.kernel || stat.start < since => continue,
            Ok(_) => {}
            Err(e) if gone(&e) => continue,
            Err(e) => return Holders::unknown(pid, "stat", &e),
        }
        let tids = match procs.tasks(pid) {
            Ok(tids) => tids,
            Err(e) if gone(&e) => continue,
            Err(e) => return Holders::unknown(pid, "task", &e),
        };
        for tid in tids {
            let task = Task { pid, tid };
            let unreadable = match holds(procs, task, master) {
                Ok(false) => continue,
                Ok(true) => return Holders::Other(pid),
                Err(e) if gone(&e) => continue,
                Err(e) => e,
            };
            // Re-read after the failure, since the listing can predate the
            // task's exit. Only a confirmed exit, zombie or another user
            // clears it; any other failed read stays unknown.
            match procs.stat(task) {
                Ok(now) if now.ending => continue,
                Ok(_) => {}
                Err(e) if gone(&e) => continue,
                Err(e) => return Holders::unknown(pid, "stat", &e),
            }
            match procs.real_uid(task) {
                Ok(Some(real)) if real != uid => continue,
                Ok(_) => return Holders::unknown(pid, "fd", &unreadable),
                Err(e) if gone(&e) => continue,
                Err(e) => return Holders::unknown(pid, "status", &e),
            }
        }
    }
    Holders::NoneFound
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
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
        fn tasks(&self, pid: u32) -> io::Result<Vec<u32>> {
            Proc.tasks(pid)
        }
        fn stat(&self, task: Task) -> io::Result<Stat> {
            Proc.stat(task)
        }
        fn real_uid(&self, task: Task) -> io::Result<Option<u32>> {
            Proc.real_uid(task)
        }
        fn fds(&self, task: Task) -> io::Result<Vec<String>> {
            Proc.fds(task)
        }
        fn link(&self, task: Task, fd: &str) -> io::Result<Vec<u8>> {
            Proc.link(task, fd)
        }
        fn file(&self, task: Task, fd: &str) -> io::Result<(u64, u64)> {
            Proc.file(task, fd)
        }
        fn index(&self, task: Task, fd: &str) -> io::Result<Option<u32>> {
            Proc.index(task, fd)
        }
    }

    fn real_scan(master: &MasterId, since: u64, pids: &[u32]) -> Holders {
        // SAFETY: `getuid` has no memory effects.
        let uid = unsafe { libc::getuid() };
        scan(&Only(pids.to_vec()), master, since, std::process::id(), uid)
    }

    /// `command` with `fd` inherited (no close-on-exec), as an exec in
    /// portable-pty's window would have it.
    fn inheritor(fd: RawFd, command: &mut std::process::Command) -> std::process::Child {
        // SAFETY: clearing FD_CLOEXEC on a descriptor this test owns.
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        let child = command.spawn().unwrap();
        // SAFETY: as above.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        child
    }

    /// A child killed and reaped when the test ends, a failed assertion
    /// included, so a RED run leaves no fixture process behind.
    struct Reaped(std::process::Child);

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn sleeper(fd: RawFd) -> std::process::Child {
        inheritor(fd, std::process::Command::new("sleep").arg("30"))
    }

    fn until(what: &str, mut done: impl FnMut() -> bool) {
        let begun = std::time::Instant::now();
        while !done() {
            assert!(
                begun.elapsed() < std::time::Duration::from_secs(5),
                "{what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_master_held_only_here_finds_none_and_an_inheritor_is_found() {
        let since = boot_ticks().unwrap();
        let (master, _slave) = openpty();
        let id = master_id(master.as_raw_fd()).unwrap();
        let me = std::process::id();
        assert_eq!(
            real_scan(&id, since, &[me]),
            Holders::NoneFound,
            "this process is skipped"
        );
        let mut child = sleeper(master.as_raw_fd());
        let pids = [me, child.id()];
        assert_eq!(real_scan(&id, since, &pids), Holders::Other(child.id()));
        // Another master isn't this one.
        let (other, _other_slave) = openpty();
        let other = master_id(other.as_raw_fd()).unwrap();
        assert_ne!(other, id);
        assert_eq!(real_scan(&other, since, &pids), Holders::NoneFound);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(real_scan(&id, since, &pids), Holders::NoneFound);
    }

    #[test]
    fn a_process_started_before_the_master_is_not_read() {
        let (master, _slave) = openpty();
        let id = master_id(master.as_raw_fd()).unwrap();
        let mut child = sleeper(master.as_raw_fd());
        // Settle on a later tick, so the child started before `since`.
        let start = boot_ticks().unwrap();
        until("the next tick", || boot_ticks().unwrap() > start + 1);
        let since = boot_ticks().unwrap();
        assert_eq!(real_scan(&id, since, &[child.id()]), Holders::NoneFound);
        assert_eq!(real_scan(&id, 0, &[child.id()]), Holders::Other(child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The reviewers' leader-exit control (Q35-20a review, U2-02): an
    /// inheritor's leader leaves with the raw `exit` syscall while a sibling
    /// thread keeps the shared table. The leader is a zombie whose own table
    /// can't be listed; the sibling's can, and holds the master.
    #[test]
    fn a_live_thread_after_its_leader_exited_is_found() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIP: python3 is not available");
            return;
        }
        // The script's raw `exit` syscall number is known for these two.
        if !matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
            eprintln!(
                "SKIP: no exit syscall number for {}",
                std::env::consts::ARCH
            );
            return;
        }
        let since = boot_ticks().unwrap();
        let (master, _slave) = openpty();
        let id = master_id(master.as_raw_fd()).unwrap();
        let script = "import ctypes, os, threading, time\n\
            threading.Thread(target=lambda: time.sleep(30), daemon=True).start()\n\
            time.sleep(0.2)\n\
            nr = {'x86_64': 60, 'aarch64': 93}[os.uname().machine]\n\
            ctypes.CDLL(None).syscall(nr, 0)\n";
        let child = Reaped(inheritor(
            master.as_raw_fd(),
            std::process::Command::new("python3").args(["-c", script]),
        ));
        let pid = child.0.id();
        until("the leader to be a zombie with its sibling alive", || {
            Proc.stat(Task { pid, tid: pid }).is_ok_and(|s| s.ending)
                && Proc.tasks(pid).is_ok_and(|t| t.len() >= 2)
        });
        assert_eq!(real_scan(&id, since, &[pid]), Holders::Other(pid));
    }

    /// The collector every real `/proc` listing goes through keeps an entry
    /// error that follows entries it read (Q35-20a fix review, U3): a
    /// dropped one would list a partial table as the whole.
    #[test]
    fn a_directory_entry_error_after_read_entries_is_the_listing() {
        let entries = |rest: io::Result<std::ffi::OsString>| {
            [Ok("1".into()), rest, Ok("3".into())].into_iter()
        };
        assert_eq!(
            entry_names(entries(Ok("2".into()))).unwrap(),
            ["1", "2", "3"]
        );
        let error = entry_names(entries(Err(io::Error::from_raw_os_error(libc::EIO)))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    }

    /// `fdinfo` text the kernel writes for a `ptmx` descriptor, and text it
    /// doesn't: an absent or malformed `tty-index` is `None`, which a
    /// matching descriptor reads as Unknown, never as an index.
    #[test]
    fn fdinfo_reads_its_tty_index_and_nothing_else() {
        let fdinfo = |index: &str| format!("pos:\t0\nflags:\t02\nmnt_id:\t30\nino:\t7\n{index}");
        assert_eq!(parse_tty_index(&fdinfo("tty-index:\t5\n")), Some(5));
        assert_eq!(parse_tty_index(&fdinfo("tty-index:\t12")), Some(12));
        for malformed in [
            "",
            "tty-index:\n",
            "tty-index:\t\n",
            "tty-index:\tx\n",
            "tty-index:\t-1\n",
            "tty-index:\t5x\n",
            "tty-index:\t4294967296\n",
            "tty_index:\t5\n",
        ] {
            assert_eq!(parse_tty_index(&fdinfo(malformed)), None, "{malformed:?}");
        }
    }

    #[test]
    fn stat_reads_this_process_and_a_zombie() {
        let me = std::process::id();
        let mine = Proc.stat(Task { pid: me, tid: me }).unwrap();
        assert!(!mine.kernel && !mine.ending);
        assert!(mine.start <= boot_ticks().unwrap());
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        until("the child to be a zombie", || {
            Proc.stat(Task { pid, tid: pid }).unwrap().ending
        });
        child.wait().unwrap();
    }

    /// Not a gate (Q35-20a review, U3): what the census costs on this
    /// machine, on the reader thread of a pane with a frontier pending. Run
    /// by hand: `cargo test --lib census_latency -- --ignored --nocapture`.
    /// Three figures: the census as the reader runs it for a generation just
    /// spawned; the same with `since` zero, where it stops at the first
    /// process it can't read; and the whole walk, every task of every
    /// process with read errors ignored, which bounds a census of a long-lived
    /// generation on this machine.
    #[test]
    #[ignore = "measurement, run by hand (Q35-20a review U3)"]
    fn census_latency() {
        let since = boot_ticks().unwrap();
        let (master, _slave) = openpty();
        let id = master_id(master.as_raw_fd()).unwrap();
        let pids = Proc.pids().unwrap();
        let tasks: usize = pids
            .iter()
            .map(|p| Proc.tasks(*p).map_or(0, |t| t.len()))
            .sum();
        println!(
            "processes {}, tasks {tasks}, load {}",
            pids.len(),
            std::fs::read_to_string("/proc/loadavg")
                .unwrap_or_default()
                .trim()
        );
        let report = |label: &str, mut run: Box<dyn FnMut() -> String>| {
            let mut times = Vec::new();
            let mut last = String::new();
            for _ in 0..30 {
                let t = std::time::Instant::now();
                last = run();
                times.push(t.elapsed());
            }
            times.sort();
            println!(
                "{label}: p50 {:?}, p90 {:?}, max {:?}; {last}",
                times[times.len() / 2],
                times[times.len() * 9 / 10],
                times[times.len() - 1]
            );
        };
        report(
            "census since this master",
            Box::new(|| format!("{:?}", holders(&id, since))),
        );
        report(
            "census since boot",
            Box::new(|| format!("{:?}", holders(&id, 0))),
        );
        report(
            "whole walk",
            Box::new(|| {
                let mut read = 0;
                for pid in Proc.pids().unwrap() {
                    for tid in Proc.tasks(pid).unwrap_or_default() {
                        if holds(&Proc, Task { pid, tid }, &id).is_ok() {
                            read += 1;
                        }
                    }
                }
                format!("{read} task tables read")
            }),
        );
    }

    // A scripted `/proc`, one read at a time; errors are errnos.

    #[derive(Clone)]
    struct FakeFd {
        name: &'static str,
        link: Result<&'static str, i32>,
        file: Result<(u64, u64), i32>,
        index: Result<Option<u32>, i32>,
    }

    struct FakeTask {
        /// Each stat in the order it's read; the last repeats.
        stats: RefCell<Vec<Result<Stat, i32>>>,
        uid: Result<Option<u32>, i32>,
        fds: Result<Vec<FakeFd>, i32>,
    }

    /// A task's script: its tid, its stats in read order, its real uid
    /// and its descriptors.
    type TaskScript = (
        u32,
        Vec<Result<Stat, i32>>,
        Result<Option<u32>, i32>,
        Result<Vec<FakeFd>, i32>,
    );

    #[derive(Default)]
    struct Fake {
        pids: Option<i32>,
        order: Vec<u32>,
        tasks: HashMap<u32, Result<Vec<u32>, i32>>,
        task: HashMap<(u32, u32), FakeTask>,
    }

    const UID: u32 = 1000;
    const ME: u32 = 1;
    const SINCE: u64 = 500;
    const MASTER: MasterId = MasterId {
        dev: 1,
        ino: 2,
        index: 3,
    };
    const PTMX: &str = "/dev/pts/ptmx";

    fn errno<T>(e: i32) -> io::Result<T> {
        Err(io::Error::from_raw_os_error(e))
    }

    fn live() -> Result<Stat, i32> {
        Ok(Stat {
            start: SINCE,
            kernel: false,
            ending: false,
        })
    }

    fn ending() -> Result<Stat, i32> {
        Ok(Stat {
            start: SINCE,
            kernel: false,
            ending: true,
        })
    }

    fn holding() -> FakeFd {
        FakeFd {
            name: "5",
            link: Ok(PTMX),
            file: Ok((MASTER.dev, MASTER.ino)),
            index: Ok(Some(MASTER.index)),
        }
    }

    fn plain() -> FakeFd {
        FakeFd {
            name: "1",
            link: Ok("/dev/pts/9"),
            file: Ok((7, 7)),
            index: Ok(None),
        }
    }

    impl Fake {
        /// Process `pid` whose single task (the leader) has `stats`, uid
        /// `uid` and descriptors `fds`.
        fn with(
            self,
            pid: u32,
            stats: Vec<Result<Stat, i32>>,
            fds: Result<Vec<FakeFd>, i32>,
        ) -> Self {
            self.with_tasks(pid, vec![(pid, stats, Ok(Some(UID)), fds)])
        }

        fn with_tasks(mut self, pid: u32, tasks: Vec<TaskScript>) -> Self {
            self.order.push(pid);
            self.tasks
                .insert(pid, Ok(tasks.iter().map(|(tid, ..)| *tid).collect()));
            for (tid, stats, uid, fds) in tasks {
                self.task.insert(
                    (pid, tid),
                    FakeTask {
                        stats: RefCell::new(stats),
                        uid,
                        fds,
                    },
                );
            }
            self
        }

        fn scan(&self) -> Holders {
            scan(self, &MASTER, SINCE, ME, UID)
        }

        fn fd(&self, task: Task, name: &str) -> FakeFd {
            self.task[&(task.pid, task.tid)]
                .fds
                .as_ref()
                .unwrap()
                .iter()
                .find(|fd| fd.name == name)
                .unwrap()
                .clone()
        }
    }

    impl Procs for Fake {
        fn pids(&self) -> io::Result<Vec<u32>> {
            match self.pids {
                Some(e) => errno(e),
                None => Ok(self.order.clone()),
            }
        }
        fn tasks(&self, pid: u32) -> io::Result<Vec<u32>> {
            self.tasks[&pid].clone().or_else(errno)
        }
        fn stat(&self, task: Task) -> io::Result<Stat> {
            let mut stats = self.task[&(task.pid, task.tid)].stats.borrow_mut();
            let next = if stats.len() > 1 {
                stats.remove(0)
            } else {
                stats[0]
            };
            next.or_else(errno)
        }
        fn real_uid(&self, task: Task) -> io::Result<Option<u32>> {
            self.task[&(task.pid, task.tid)].uid.or_else(errno)
        }
        fn fds(&self, task: Task) -> io::Result<Vec<String>> {
            match &self.task[&(task.pid, task.tid)].fds {
                Ok(fds) => Ok(fds.iter().map(|fd| fd.name.to_owned()).collect()),
                Err(e) => errno(*e),
            }
        }
        fn link(&self, task: Task, fd: &str) -> io::Result<Vec<u8>> {
            self.fd(task, fd)
                .link
                .map(|l| l.as_bytes().to_vec())
                .or_else(errno)
        }
        fn file(&self, task: Task, fd: &str) -> io::Result<(u64, u64)> {
            self.fd(task, fd).file.or_else(errno)
        }
        fn index(&self, task: Task, fd: &str) -> io::Result<Option<u32>> {
            self.fd(task, fd).index.or_else(errno)
        }
    }

    fn unknown(pid: u32, step: &'static str, e: i32) -> Holders {
        Holders::Unknown {
            pid,
            step,
            errno: Some(e),
        }
    }

    #[test]
    fn the_census_skips_this_process_old_processes_and_kernel_threads() {
        let old = Ok(Stat {
            start: SINCE - 1,
            kernel: false,
            ending: false,
        });
        let kernel = Ok(Stat {
            start: SINCE + 9,
            kernel: true,
            ending: false,
        });
        let fake = Fake::default()
            .with(ME, vec![live()], Ok(vec![holding()]))
            .with(10, vec![old], Ok(vec![holding()]))
            .with(11, vec![kernel], Err(libc::EACCES))
            .with(12, vec![live()], Ok(vec![plain()]));
        assert_eq!(fake.scan(), Holders::NoneFound);
        let fake = fake.with(13, vec![live()], Ok(vec![plain(), holding()]));
        assert_eq!(
            fake.scan(),
            Holders::Other(13),
            "a holder started at `since`"
        );
    }

    #[test]
    fn what_went_away_holds_nothing() {
        let fd = |link, file, index| FakeFd {
            link,
            file,
            index,
            ..holding()
        };
        for (n, fake) in [
            Fake::default().with(20, vec![Err(libc::ESRCH)], Ok(vec![holding()])),
            Fake::default().with(21, vec![live()], Err(libc::ENOENT)),
            Fake::default().with(
                22,
                vec![live()],
                Ok(vec![fd(Err(libc::ENOENT), Ok((1, 2)), Ok(Some(3)))]),
            ),
            Fake::default().with(
                23,
                vec![live()],
                Ok(vec![fd(Ok(PTMX), Err(libc::ESRCH), Ok(Some(3)))]),
            ),
            Fake::default().with(
                24,
                vec![live()],
                Ok(vec![fd(Ok(PTMX), Ok((1, 2)), Err(libc::ENOENT))]),
            ),
            {
                let mut fake = Fake::default().with(25, vec![live()], Ok(vec![holding()]));
                fake.tasks.insert(25, Err(libc::ESRCH));
                fake
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(fake.scan(), Holders::NoneFound, "case {n}");
        }
    }

    /// The reviewers' U2-01 rows: every read that fails other than by going
    /// away is unknown, at each level of the census.
    #[test]
    fn every_other_failed_read_is_unknown() {
        let fd = |link, file, index| FakeFd {
            link,
            file,
            index,
            ..holding()
        };
        let mut listing = Fake::default().with(30, vec![live()], Ok(vec![holding()]));
        listing.pids = Some(libc::EIO);
        assert_eq!(listing.scan(), unknown(0, "proc", libc::EIO));
        let mut tasks = Fake::default().with(31, vec![live()], Ok(vec![holding()]));
        tasks.tasks.insert(31, Err(libc::EACCES));
        assert_eq!(tasks.scan(), unknown(31, "task", libc::EACCES));
        assert_eq!(
            Fake::default()
                .with(32, vec![Err(libc::EIO)], Ok(vec![]))
                .scan(),
            unknown(32, "stat", libc::EIO)
        );
        for (pid, desc, e) in [
            (
                33,
                fd(Err(libc::EACCES), Ok((1, 2)), Ok(Some(3))),
                libc::EACCES,
            ),
            (34, fd(Ok(PTMX), Err(libc::EIO), Ok(Some(3))), libc::EIO),
            (
                35,
                fd(Ok(PTMX), Ok((1, 2)), Err(libc::EACCES)),
                libc::EACCES,
            ),
        ] {
            let fake = Fake::default().with(pid, vec![live()], Ok(vec![plain(), desc]));
            assert_eq!(fake.scan(), unknown(pid, "fd", e), "pid {pid}");
        }
        // This master's inode with no tty-index: unknown, not another master.
        let fake = Fake::default().with(
            36,
            vec![live()],
            Ok(vec![fd(Ok(PTMX), Ok((1, 2)), Ok(None))]),
        );
        assert!(matches!(
            fake.scan(),
            Holders::Unknown {
                pid: 36,
                step: "fd",
                errno: None
            }
        ));
        // The descriptor table can't be listed and the task is live and this
        // user's.
        let fake = Fake::default().with(37, vec![live()], Err(libc::EACCES));
        assert_eq!(fake.scan(), unknown(37, "fd", libc::EACCES));
        // The reviewers' row: live, the table unreadable, then the re-read
        // stat fails other than by going away.
        let fake = Fake::default().with(38, vec![live(), Err(libc::EIO)], Err(libc::EACCES));
        assert_eq!(fake.scan(), unknown(38, "stat", libc::EIO));
        // The real uid can't be read, or isn't there.
        let fake = Fake::default().with_tasks(
            39,
            vec![(39, vec![live()], Err(libc::EIO), Err(libc::EACCES))],
        );
        assert_eq!(fake.scan(), unknown(39, "status", libc::EIO));
        let fake =
            Fake::default().with_tasks(40, vec![(40, vec![live()], Ok(None), Err(libc::EACCES))]);
        assert_eq!(fake.scan(), unknown(40, "fd", libc::EACCES));
    }

    #[test]
    fn an_unreadable_task_is_skipped_only_when_it_ended_went_or_is_another_users() {
        for (n, fake) in [
            Fake::default().with(50, vec![live(), ending()], Err(libc::EACCES)),
            Fake::default().with(51, vec![live(), Err(libc::ESRCH)], Err(libc::EACCES)),
            Fake::default().with_tasks(
                52,
                vec![(52, vec![live()], Ok(Some(UID + 1)), Err(libc::EACCES))],
            ),
            Fake::default().with_tasks(
                53,
                vec![(53, vec![live()], Err(libc::ENOENT), Err(libc::EACCES))],
            ),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(fake.scan(), Holders::NoneFound, "case {n}");
        }
    }

    /// U2-02 in the script: a leader that exited (unreadable, ending) is
    /// skipped as a task, and its live sibling's table is read and holds the
    /// master; a sibling whose table can't be read is unknown.
    #[test]
    fn a_leaders_exit_is_not_its_processs() {
        let fake = Fake::default().with_tasks(
            60,
            vec![
                (60, vec![ending()], Ok(Some(UID)), Err(libc::EACCES)),
                (61, vec![live()], Ok(Some(UID)), Ok(vec![holding()])),
            ],
        );
        assert_eq!(fake.scan(), Holders::Other(60));
        let fake = Fake::default().with_tasks(
            62,
            vec![
                (62, vec![ending()], Ok(Some(UID)), Err(libc::EACCES)),
                (63, vec![live()], Ok(Some(UID)), Err(libc::EACCES)),
            ],
        );
        assert_eq!(fake.scan(), unknown(62, "fd", libc::EACCES));
        // A thread with its own table holding the master, the leader's not.
        let fake = Fake::default().with_tasks(
            64,
            vec![
                (64, vec![live()], Ok(Some(UID)), Ok(vec![plain()])),
                (65, vec![live()], Ok(Some(UID)), Ok(vec![holding()])),
            ],
        );
        assert_eq!(fake.scan(), Holders::Other(64));
    }
}
