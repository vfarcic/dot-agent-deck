//! Reading `/proc`: the processes one sample accounts, as
//! [`ProcessSample`]s for [`Aggregator::step`].

use std::fs;
use std::io;

use super::aggregate::{Aggregator, Environ, ProcessSample};
use super::identity::run_identity;
use super::procfs::{ProcStat, Pss, parse_effective_uid};

/// Whose processes the reader takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Users {
    /// The collector's own effective user.
    pub own: u32,
    /// Every user's processes it can read (`--all-users`), not only `own`'s.
    pub all: bool,
}

/// Every pid `/proc` lists now.
pub fn pids() -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect()
}

/// The visible processes among `pids`, read in the order that keeps a
/// child's reaping mid-sample least likely to be counted twice: every `stat`
/// first, then each one's user and the environments that are not settled
/// yet, then `smaps_rollup`.
///
/// A process is visible when it is not a kernel thread, its effective user
/// (from `status`, not the owner of `/proc/<pid>`, which is root for a
/// non-dumpable process) is `users.own` or `users.all` is set, and its
/// environment can be read. One of the collector's own user whose
/// environment it may not read (a process that made itself non-dumpable) is
/// visible too, as [`Environ::Unreadable`]: it can take its parent's run.
pub fn read_processes(
    aggregator: &Aggregator,
    users: Users,
    pids: impl IntoIterator<Item = u32>,
) -> Vec<ProcessSample> {
    let mut stats = Vec::new();
    for pid in pids {
        if let Ok(text) = fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(stat) = ProcStat::parse(&text)
            && !stat.is_kernel_thread()
        {
            stats.push((pid, stat));
        }
    }
    let mut visible = Vec::with_capacity(stats.len());
    for (pid, stat) in stats {
        let Some(uid) = fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|text| parse_effective_uid(&text))
        else {
            continue;
        };
        if !users.all && uid != users.own {
            continue;
        }
        let environ = if aggregator.environ_settled(pid, stat.starttime, uid) {
            Environ::NotRead
        } else {
            match fs::read(format!("/proc/{pid}/environ")) {
                Ok(bytes) => Environ::Read(run_identity(&bytes)),
                Err(e) if uid == users.own && denied(&e) => Environ::Unreadable,
                // Another user's that it may not read is not visible, and
                // counted nowhere; one that vanished is gone.
                Err(_) => continue,
            }
        };
        visible.push((pid, uid, stat, environ));
    }
    visible
        .into_iter()
        .map(|(pid, uid, stat, environ)| ProcessSample {
            pid,
            uid,
            stat,
            environ,
            pss: fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
                .ok()
                .and_then(|text| Pss::parse_smaps_rollup(&text)),
        })
        .collect()
}

fn denied(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM))
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::telemetry::collector::procfs::parse_boot_time;

    /// Kills and reaps the child it holds when dropped, so a failed
    /// assertion leaves nothing running. A child that is not reaped keeps its
    /// pid, so the pid names no other process.
    struct KillChild(libc::pid_t);

    impl Drop for KillChild {
        fn drop(&mut self) {
            // SAFETY: plain syscalls on a child this test forked and has not
            // reaped.
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
                libc::waitpid(self.0, std::ptr::null_mut(), 0);
            }
        }
    }

    /// Kills the process a pidfd refers to when dropped. Unlike a numeric
    /// pid, a pidfd cannot come to name another process once its own exits.
    struct KillPidfd(OwnedFd);

    impl KillPidfd {
        /// A pidfd for `pid`, which the caller knows names the process it
        /// means: the guard kills whatever process the pidfd names.
        fn open(pid: libc::pid_t) -> io::Result<KillPidfd> {
            // SAFETY: `pidfd_open` takes a pid and flags and returns a new
            // descriptor or -1.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` is a descriptor this test now owns.
            Ok(KillPidfd(unsafe {
                OwnedFd::from_raw_fd(fd as libc::c_int)
            }))
        }

        /// Send `signal`; `false` once the process has exited.
        fn signal(&self, signal: libc::c_int) -> bool {
            // SAFETY: the pidfd is open; a null `siginfo` is allowed.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            rc == 0
        }
    }

    impl Drop for KillPidfd {
        fn drop(&mut self) {
            self.signal(libc::SIGKILL);
        }
    }

    /// Whether the last failed call of this thread was interrupted by a
    /// signal. Reads `errno` only, so a forked child may call it.
    fn interrupted() -> bool {
        // SAFETY: `__errno_location` returns this thread's `errno`.
        unsafe { *libc::__errno_location() == libc::EINTR }
    }

    /// Write all of `bytes` to `fd`, retrying interrupted writes; `false`
    /// when a write fails otherwise. It calls `write` only and allocates
    /// nothing, so a forked child may call it.
    fn write_all_raw(fd: libc::c_int, mut bytes: &[u8]) -> bool {
        while !bytes.is_empty() {
            // SAFETY: `bytes` is valid for its length.
            let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            if n > 0 {
                bytes = &bytes[n as usize..];
            } else if !(n < 0 && interrupted()) {
                return false;
            }
        }
        true
    }

    /// Fill `buf` from `fd`, retrying interrupted reads; `false` at end of
    /// file or when a read fails otherwise. It calls `read` only and
    /// allocates nothing, so a forked child may call it.
    fn read_exact_raw(fd: libc::c_int, mut buf: &mut [u8]) -> bool {
        while !buf.is_empty() {
            // SAFETY: `buf` is valid for writes of its length.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                buf = &mut buf[n as usize..];
            } else if !(n < 0 && interrupted()) {
                return false;
            }
        }
        true
    }

    /// What the parent tells a launcher over its handover pipe.
    const HANDED_OVER: u8 = 1;
    const ABANDONED: u8 = 0;

    /// A launcher that still answers for the burner it forked. Dropped
    /// before [`Launcher::hand_over`], it tells the launcher the setup was
    /// abandoned, which makes the launcher kill and reap its burner and
    /// exit, then reaps the launcher, killing it only if it has not exited
    /// within the bound.
    struct Launcher {
        pid: libc::pid_t,
        handover: Option<OwnedFd>,
    }

    impl Launcher {
        /// Tell the launcher the parent holds the burner's pidfd, so from
        /// here on the launcher only waits to be killed, and return the guard
        /// that kills and reaps it.
        fn hand_over(mut self) -> KillChild {
            let handover = self.handover.as_ref().unwrap();
            if !write_all_raw(handover.as_raw_fd(), &[HANDED_OVER]) {
                // Dropping `self` abandons the setup instead.
                panic!(
                    "the launcher did not take the handover: {}",
                    io::Error::last_os_error()
                );
            }
            self.handover = None;
            KillChild(self.pid)
        }
    }

    impl Drop for Launcher {
        fn drop(&mut self) {
            let Some(handover) = self.handover.take() else {
                return;
            };
            // Closing the pipe tells it too, should this write fail.
            write_all_raw(handover.as_raw_fd(), &[ABANDONED]);
            drop(handover);
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                // SAFETY: a non-blocking wait on a child that has not been
                // reaped.
                if unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), libc::WNOHANG) } != 0 {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(KillChild(self.pid));
        }
    }

    /// Whether this process holds `CAP_SYS_PTRACE` in its effective set
    /// (bit 19 of `CapEff:` in its `status`).
    fn has_cap_sys_ptrace() -> bool {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let eff = status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:"))
            .unwrap();
        u64::from_str_radix(eff.trim(), 16).unwrap() & (1 << 19) != 0
    }

    fn own_uid() -> u32 {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    fn clk_tck() -> u64 {
        // SAFETY: `sysconf` reads a constant and has no preconditions.
        u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap()
    }

    fn aggregator() -> Aggregator {
        let boot = parse_boot_time(&fs::read_to_string("/proc/stat").unwrap()).unwrap();
        Aggregator::new(clk_tck(), boot, 0)
    }

    /// Wait for `ready` to hold. The bound is generous: it only decides how
    /// long a broken setup takes to fail.
    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stat_of(pid: u32) -> Option<ProcStat> {
        ProcStat::parse(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    /// U2/V5: a process of the collector's own user that made itself
    /// non-dumpable has a root-owned `/proc/<pid>` and an environment that
    /// another process of its user may not read; it is still the
    /// collector's user's, and is read.
    #[test]
    fn a_non_dumpable_process_of_the_own_user_is_read_as_that_user() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for the two descriptors.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: each descriptor is open and owned only here.
        let (ack, ack_write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), fds[1]) };
        // SAFETY: the child makes syscalls only and never returns into Rust;
        // the parent kills and reaps it.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // SAFETY: the forked child, calling async-signal-safe syscalls.
            unsafe {
                // 1 once it is non-dumpable, 0 when `prctl` failed.
                let done: u8 =
                    u8::from(libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong) == 0);
                write_all_raw(ack_write, &[done]);
                loop {
                    libc::pause();
                }
            }
        }
        let _kill = KillChild(pid);
        // SAFETY: the parent's copy of the write end, closed once.
        unsafe { libc::close(ack_write) };
        let mut done = [0u8];
        let read = read_exact_raw(ack.as_raw_fd(), &mut done);
        assert!(read && done == [1], "the child did not become non-dumpable");
        let pid = pid as u32;
        let own = own_uid();
        if own != 0 {
            // Its `/proc` files turn root's once it is non-dumpable (and, on
            // some kernels, `/proc/<pid>` itself, which is why the user is
            // not read from that).
            let owner = fs::metadata(format!("/proc/{pid}/environ")).unwrap().uid();
            assert_eq!(owner, 0);
        }

        let read = read_processes(&aggregator(), Users { own, all: false }, [pid]);
        assert_eq!(read.len(), 1, "{read:?}");
        assert_eq!(read[0].pid, pid);
        assert_eq!(read[0].uid, own);
        // Another process of its user without `CAP_SYS_PTRACE` is refused
        // its environment (as uid 1000 on the dev box and in RWX). Whether
        // root, or a holder of that capability, is refused varies (root
        // without it read the environment in RWX, and a security module
        // can refuse one with it), so there the expected result is what
        // reading it directly gives.
        let readable =
            (own == 0 || has_cap_sys_ptrace()) && fs::read(format!("/proc/{pid}/environ")).is_ok();
        if readable {
            assert!(matches!(read[0].environ, Environ::Read(_)), "{read:?}");
        } else {
            assert_eq!(read[0].environ, Environ::Unreadable);
        }
        // Not the collector's user: not read.
        let other = Users {
            own: own.wrapping_add(1),
            all: false,
        };
        assert!(read_processes(&aggregator(), other, [pid]).is_empty());
    }

    /// R6: a process that cleared its environment and left the tree (double
    /// fork) is read, and lands in the unattributed bucket with its CPU and
    /// its anonymous memory.
    #[test]
    fn a_cleared_reparented_process_is_counted_unattributed() {
        let marker = format!("deck.run.id=reader-test-{}", std::process::id());
        // The burner starts as a shell holding the run's label and execs,
        // through `env -i`, a shell with an empty environment. Everything
        // the forked children use is allocated before the fork.
        let sh = CString::new("/bin/sh").unwrap();
        let script = CString::new("exec env -i /bin/sh -c 'while :; do :; done'").unwrap();
        let argv = [
            c"sh".as_ptr(),
            c"-c".as_ptr(),
            script.as_ptr(),
            std::ptr::null(),
        ];
        let label = CString::new(format!("OTEL_RESOURCE_ATTRIBUTES={marker}")).unwrap();
        let mut path = b"PATH=".to_vec();
        path.extend_from_slice(
            std::env::var_os("PATH")
                .unwrap_or_else(|| "/usr/bin:/bin".into())
                .as_encoded_bytes(),
        );
        let path = CString::new(path).unwrap();
        let envp = [label.as_ptr(), path.as_ptr(), std::ptr::null()];
        let pipe = || {
            let mut fds = [0 as libc::c_int; 2];
            // SAFETY: `fds` has room for the two descriptors.
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            // SAFETY: each descriptor is open and owned only here.
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
        };
        let (told, tell) = pipe();
        let (handed, handover) = pipe();
        let (tell_fd, handed_fd) = (tell.as_raw_fd(), handed.as_raw_fd());
        // The launcher forks the burner and tells its pid, never reaping it,
        // so while the launcher lives that pid names the burner and no other
        // process. Until the parent hands over, saying it holds the burner's
        // pidfd, the launcher answers for the burner: when the parent
        // abandons the setup, or its end of the pipe closes, the launcher
        // kills and reaps the burner and exits. After the handover it waits
        // to be killed.
        // SAFETY: the forked children make syscalls only, on descriptors
        // and buffers made before the fork, and never return into Rust; the
        // parent reaps the launcher.
        let launcher = unsafe { libc::fork() };
        assert!(launcher >= 0, "fork failed");
        if launcher == 0 {
            // SAFETY: the forked child, calling async-signal-safe syscalls.
            unsafe {
                // Its copies of the parent's ends, so that the parent's end
                // closing reads as the end of the pipe here.
                libc::close(told.as_raw_fd());
                libc::close(handover.as_raw_fd());
                // An ignored SIGCHLD would reap the burner when it exits.
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                let burner = libc::fork();
                if burner == 0 {
                    libc::execve(sh.as_ptr(), argv.as_ptr(), envp.as_ptr());
                    libc::_exit(127);
                }
                let sent = write_all_raw(tell_fd, &burner.to_ne_bytes());
                if burner < 0 {
                    libc::_exit(1);
                }
                let mut said = [ABANDONED];
                if !(sent && read_exact_raw(handed_fd, &mut said) && said == [HANDED_OVER]) {
                    libc::kill(burner, libc::SIGKILL);
                    while libc::waitpid(burner, std::ptr::null_mut(), 0) < 0 && interrupted() {}
                    libc::_exit(1);
                }
                loop {
                    libc::pause();
                }
            }
        }
        let launcher = Launcher {
            pid: launcher,
            handover: Some(handover),
        };
        drop((tell, handed));
        let mut told_pid = [0u8; 4];
        let read = read_exact_raw(told.as_raw_fd(), &mut told_pid);
        let pid = libc::pid_t::from_ne_bytes(told_pid);
        assert!(read && pid > 0, "the launcher did not start the burner");
        // The launcher has not been handed over to, so it is still running
        // unless killed, and the burner, its unreaped child, still holds
        // `pid`. A failure here drops `launcher`, which has the launcher
        // kill and reap the burner.
        let burner = KillPidfd::open(pid).unwrap_or_else(|e| panic!("pidfd_open: {e}"));
        // SAFETY: a non-blocking wait on a child that has not been reaped.
        let waited = unsafe { libc::waitpid(launcher.pid, std::ptr::null_mut(), libc::WNOHANG) };
        if waited != 0 {
            // Then the launcher was killed or exited (and, unless the wait
            // failed, is reaped), so neither the pidfd nor its pid is known
            // to name the process meant, and neither guard may kill.
            std::mem::forget(burner);
            std::mem::forget(launcher);
            panic!("the launcher exited before the pidfd was taken ({waited})");
        }
        let launcher = launcher.hand_over();
        // The burner leaves the tree: its parent exits and is reaped.
        let launcher_pid = launcher.0 as u32;
        drop(launcher);
        let pid = pid as u32;
        // Ready once it is the final shell with the cleared environment
        // (not the shell or `env` it ran through), and it has used a tick
        // of CPU.
        let shell = fs::canonicalize("/bin/sh").unwrap();
        let marker = marker.as_bytes();
        wait_for("the burner to be cleared and running", || {
            stat_of(pid).is_some_and(|stat| stat.total_ticks() > 0)
                && fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe == shell)
                && fs::read(format!("/proc/{pid}/environ"))
                    .is_ok_and(|env| !env.windows(marker.len()).any(|w| w == marker))
        });
        assert!(burner.signal(0), "the burner exited");
        let ppid = stat_of(pid).unwrap().ppid;
        assert_ne!(ppid, launcher_pid);
        assert_ne!(ppid, std::process::id());

        let mut aggregator = aggregator();
        let users = Users {
            own: own_uid(),
            all: false,
        };
        let read = read_processes(&aggregator, users, [pid]);
        assert_eq!(read.len(), 1, "{read:?}");
        assert_eq!(read[0].environ, Environ::Read(None));
        let sample = aggregator.step(1, read);
        assert!(sample.runs.is_empty(), "{:?}", sample.runs);
        assert_eq!(sample.unattributed.processes, 1);
        assert!(sample.unattributed.cpu_seconds > 0.0);
        assert!(
            sample.unattributed.pss.anon > 0,
            "{:?}",
            sample.unattributed
        );
    }
}
