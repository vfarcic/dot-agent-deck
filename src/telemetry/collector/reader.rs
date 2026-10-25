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
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::telemetry::collector::procfs::parse_boot_time;

    /// Kills the process it holds when dropped, so a failed assertion leaves
    /// nothing running.
    struct Kill {
        pid: libc::pid_t,
        child: bool,
    }

    impl Drop for Kill {
        fn drop(&mut self) {
            // SAFETY: plain syscalls on a pid this test started.
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
                if self.child {
                    libc::waitpid(self.pid, std::ptr::null_mut(), 0);
                }
            }
        }
    }

    fn own_uid() -> u32 {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    fn aggregator() -> Aggregator {
        let boot = parse_boot_time(&fs::read_to_string("/proc/stat").unwrap()).unwrap();
        // SAFETY: `sysconf` reads a constant and has no preconditions.
        let clk_tck = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap();
        Aggregator::new(clk_tck, boot, 0)
    }

    /// Wait, at most five seconds, for `ready` to hold.
    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stat_of(pid: u32) -> Option<ProcStat> {
        ProcStat::parse(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    /// U2: a process of the collector's own user that made itself
    /// non-dumpable has a root-owned `/proc/<pid>` and an environment only
    /// root may read; it is still the collector's user's, and is read.
    #[test]
    fn a_non_dumpable_process_of_the_own_user_is_read_as_that_user() {
        // SAFETY: the child makes two syscalls and never returns into Rust;
        // the parent kills and reaps it.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // SAFETY: the forked child, calling async-signal-safe syscalls.
            unsafe {
                libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong);
                loop {
                    libc::pause();
                }
            }
        }
        let _kill = Kill { pid, child: true };
        let pid = pid as u32;
        let own = own_uid();
        // Its `/proc` files turn root's once it is non-dumpable (and, on some
        // kernels, `/proc/<pid>` itself, which is why the user is not read
        // from that).
        wait_for("the child to become non-dumpable", || {
            fs::metadata(format!("/proc/{pid}/environ")).is_ok_and(|m| m.uid() == 0)
        });

        let read = read_processes(&aggregator(), Users { own, all: false }, [pid]);
        assert_eq!(read.len(), 1, "{read:?}");
        assert_eq!(read[0].pid, pid);
        assert_eq!(read[0].uid, own);
        if own != 0 {
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
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg("(env -i /bin/sh -c 'while :; do :; done' </dev/null >/dev/null 2>&1 & echo $!)")
            .env("OTEL_RESOURCE_ATTRIBUTES", "deck.run.id=reader-test")
            .output()
            .unwrap();
        let pid: u32 = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _kill = Kill {
            pid: pid as libc::pid_t,
            child: false,
        };
        // Both shells have exited, so it has been reparented; wait until it
        // has used a tick of CPU.
        wait_for("the burner to use CPU", || {
            stat_of(pid).is_some_and(|s| s.total_ticks() > 0)
        });
        assert_ne!(stat_of(pid).unwrap().ppid, std::process::id());

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
