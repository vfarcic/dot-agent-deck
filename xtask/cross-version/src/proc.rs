//! Pid-scoped process control.
//!
//! # Why this module refuses to match patterns
//!
//! On 2026-09-15 an agent finishing with a cross-version sandbox ran
//! `pkill -f "daemon serve"`. That pattern also matches the **production**
//! daemon's command line (`…/dot-agent-deck daemon serve`), which took the
//! SIGTERM 1.42 s later and gracefully stopped nine panes across three
//! dispatched units ([#428 occurrence
//! #5](https://github.com/vfarcic/dot-agent-deck/issues/428#issuecomment-5688814518)).
//! `pkill` sends SIGTERM, so it goes *around* issue #770's `daemon stop`
//! refusal rather than having to defeat it — a compliant daemon shuts its
//! agents down cleanly and names none of them.
//!
//! So nothing here takes a name. [`SandboxProcess`] captures the pid at spawn
//! and every signal is checked against `/proc/<pid>/cmdline` first: a pid whose
//! command line does not contain the sandbox's own path is **not signalled**, it
//! is reported. A pid can be recycled between the capture and the signal, and
//! that check is what stands between a recycled pid and somebody else's work.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Whether `pid` exists, via `kill(pid, 0)` — the only call that tells "no such
/// process" (ESRCH) apart from "not yours" (EPERM).
pub fn is_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 performs the permission/existence check and
    // delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 || *libc::__errno_location() == libc::EPERM }
}

/// `/proc/<pid>/cmdline` with its NUL separators turned into spaces, or `None`
/// when the process is gone.
pub fn cmdline(pid: i32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Where `/proc/<pid>/exe` points, or `None` when the process is gone or the
/// link is unreadable.
pub fn exe_path(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// A process this harness started, remembered by pid and by the string its
/// command line must still contain before anything is signalled.
pub struct SandboxProcess {
    pub pid: i32,
    /// The substring every signal is gated on. In practice the absolute path of
    /// the sandbox binary — a path under this run's own directories, which no
    /// production deck's command line contains.
    pub identity: String,
    pub label: String,
    child: Option<std::process::Child>,
}

/// Why a signal was not sent.
#[derive(Debug)]
pub enum SignalRefusal {
    /// The pid is gone. Nothing to do, and nothing to worry about.
    Gone,
    /// The pid exists but its command line no longer contains [`identity`].
    /// Almost certainly a recycled pid; signalling it would hit a stranger.
    ///
    /// [`identity`]: SandboxProcess::identity
    NotOurs { cmdline: String },
}

impl SandboxProcess {
    pub fn adopt(child: std::process::Child, identity: String, label: String) -> Self {
        Self {
            pid: child.id() as i32,
            identity,
            label,
            child: Some(child),
        }
    }

    /// Confirm the pid is still the process this harness started.
    pub fn verify(&self) -> Result<String, SignalRefusal> {
        match cmdline(self.pid) {
            None => Err(SignalRefusal::Gone),
            Some(cmd) if cmd.contains(&self.identity) => Ok(cmd),
            Some(cmd) => Err(SignalRefusal::NotOurs { cmdline: cmd }),
        }
    }

    /// SIGTERM the process, wait up to `grace`, then SIGKILL what is left.
    ///
    /// Refuses without signalling when [`verify`](Self::verify) does.
    pub fn terminate(&mut self, grace: Duration) -> Result<(), SignalRefusal> {
        self.verify()?;
        // SAFETY: the pid was verified to still carry this sandbox's own path in
        // its command line one syscall ago.
        unsafe { libc::kill(self.pid, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !is_alive(self.pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if is_alive(self.pid) && self.verify().is_ok() {
            // SAFETY: re-verified immediately above.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        Ok(())
    }
}

/// Which pids are listening on `socket`, according to `ss`.
///
/// Returns `None` when `ss` is absent or its output cannot be parsed — the
/// caller reports that as *not checked* rather than as a pass, because a tell
/// nobody measured is not a tell.
pub fn listeners_on(socket: &Path) -> Option<Vec<i32>> {
    let out = std::process::Command::new("ss")
        .args(["-x", "-l", "-p", "-n", "--no-header"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let want = socket.to_string_lossy();
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if !line.contains(want.as_ref()) {
            continue;
        }
        // `users:(("dot-agent-deck",pid=12345,fd=7))`
        for chunk in line.split("pid=").skip(1) {
            let digits: String = chunk.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(pid) = digits.parse::<i32>() {
                pids.push(pid);
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Some(pids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_names_itself() {
        let me = std::process::id() as i32;
        assert!(is_alive(me));
        assert!(cmdline(me).is_some());
    }

    #[test]
    fn a_pid_that_cannot_exist_is_not_alive() {
        // 0 addresses the caller's own process group on Linux, so the smallest
        // safe "certainly absent" probe is a pid above the configured maximum.
        let ceiling = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(4_194_304);
        assert!(cmdline(ceiling.saturating_add(1)).is_none());
    }

    #[test]
    fn verify_refuses_a_pid_whose_cmdline_lost_the_sandbox_path() {
        let proc = SandboxProcess {
            pid: std::process::id() as i32,
            identity: "/definitely/not/in/this/cmdline-9f3a".to_string(),
            label: "self".to_string(),
            child: None,
        };
        assert!(matches!(proc.verify(), Err(SignalRefusal::NotOurs { .. })));
    }
}
