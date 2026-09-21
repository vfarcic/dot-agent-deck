//! Process identity, pid-scoped signalling, and the kernel's Unix-socket table.
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
//! So nothing here takes a name.
//!
//! # What "identity" means here
//!
//! A cmdline substring is not identity: the first version of this harness gated
//! every signal on one, and the safety audit that followed listed that as a
//! falsified absolute. [`Identity`] is what a signal is gated on now — every
//! field below is captured right after spawn and re-read immediately before any
//! signal, and a single difference refuses the signal:
//!
//! * the start time, field 22 of `/proc/<pid>/stat`, which is what tells a
//!   recycled pid apart from the process that used to hold the number;
//! * `/proc/<pid>/exe`;
//! * the exact NUL-separated command line, element by element;
//! * the working directory;
//! * the **whole** initial environment, which carries the run's unique
//!   `DAD_XVER_SANDBOX` marker and every sandbox path;
//! * the mount namespace.
//!
//! Two structural properties sit underneath those checks rather than replacing
//! them. Every process this is used on is inside the run's private PID
//! namespace (see `isolation.rs`), where no host process has a pid at all; and a
//! [`SandboxProcess`] keeps its `Child` handle un-reaped until after the last
//! signal, so the kernel cannot hand its pid to anyone else in between.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Whether `pid` exists, via `kill(pid, 0)` — the only call that tells "no such
/// process" (ESRCH) apart from "not yours" (EPERM). A zombie still exists.
pub fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs the permission/existence check and
    // delivers nothing. `pid > 0` so it can never address a process group.
    unsafe { libc::kill(pid, 0) == 0 || *libc::__errno_location() == libc::EPERM }
}

/// Every pid visible in this process's `/proc` — which, inside the run's
/// private PID namespace, is exactly the sandbox's own processes.
pub fn pids() -> Result<Vec<i32>, String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| format!("read /proc: {e}"))? {
        let Ok(entry) = entry else { continue };
        if let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        {
            out.push(pid);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// The fields of `/proc/<pid>/stat` after the `comm` field, which is
/// parenthesised and may itself contain spaces and parentheses — hence the
/// split on the LAST `)`.
fn stat_fields_after_comm(stat: &str) -> Option<Vec<&str>> {
    let close = stat.rfind(')')?;
    Some(stat[close + 1..].split_whitespace().collect())
}

/// Field 22 (`starttime`, clock ticks since boot) out of a `/proc/<pid>/stat`
/// line. After `comm` the next field is field 3 (`state`), so field 22 is at
/// index 19 of what follows.
pub fn parse_start_time(stat: &str) -> Option<u64> {
    stat_fields_after_comm(stat)?.get(19)?.parse().ok()
}

/// Field 3 (`state`) out of a `/proc/<pid>/stat` line.
pub fn parse_state(stat: &str) -> Option<char> {
    stat_fields_after_comm(stat)?.first()?.chars().next()
}

pub fn start_time(pid: i32) -> Option<u64> {
    parse_start_time(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

pub fn state(pid: i32) -> Option<char> {
    parse_state(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// `/proc/<pid>/cmdline` split on its NUL separators, or `None` when the
/// process is gone. An empty vector is a zombie or a kernel thread.
pub fn cmdline(pid: i32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

/// Where `/proc/<pid>/exe` points, or `None` when the process is gone or the
/// link is unreadable.
pub fn exe_path(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

pub fn cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The process's INITIAL environment. `/proc/<pid>/environ` reads the block the
/// kernel laid out at `execve`, so a later `setenv` in the process does not
/// move it — which is what makes it usable as an identity field.
pub fn environ(pid: i32) -> Option<BTreeMap<String, String>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| {
                let s = String::from_utf8_lossy(s);
                s.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect(),
    )
}

/// `readlink /proc/<who>/ns/<kind>`, e.g. `mnt:[4026533140]`. `who` is a pid
/// or `self`.
pub fn namespace(who: &str, kind: &str) -> Option<String> {
    std::fs::read_link(format!("/proc/{who}/ns/{kind}"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Everything a signal to a sandbox process is gated on. See the module docs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub pid: i32,
    pub start_time: u64,
    pub exe: PathBuf,
    pub cmdline: Vec<String>,
    pub cwd: PathBuf,
    pub environ: BTreeMap<String, String>,
    pub mnt_ns: String,
}

/// Why a process no longer matches its recorded [`Identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    /// The pid is gone, or is now a zombie that has already exited. Nothing to
    /// signal and nothing to worry about.
    Gone,
    /// A field differs, or could not be read at all. Either way the signal is
    /// refused: an identity check that cannot be evaluated is not a pass.
    Changed { field: &'static str, detail: String },
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mismatch::Gone => write!(f, "the process is gone"),
            Mismatch::Changed { field, detail } => write!(f, "{field} differs: {detail}"),
        }
    }
}

impl Identity {
    /// Read every identity field of `pid` now.
    pub fn capture(pid: i32) -> Result<Self, String> {
        let unreadable = |what: &str| format!("/proc/{pid}/{what} is unreadable");
        let start_time = start_time(pid).ok_or_else(|| unreadable("stat"))?;
        let exe = exe_path(pid).ok_or_else(|| unreadable("exe"))?;
        let cmdline = cmdline(pid).ok_or_else(|| unreadable("cmdline"))?;
        let cwd = cwd(pid).ok_or_else(|| unreadable("cwd"))?;
        let environ = environ(pid).ok_or_else(|| unreadable("environ"))?;
        let mnt_ns = namespace(&pid.to_string(), "mnt").ok_or_else(|| unreadable("ns/mnt"))?;
        Ok(Self {
            pid,
            start_time,
            exe,
            cmdline,
            cwd,
            environ,
            mnt_ns,
        })
    }

    /// Capture a process the harness has JUST spawned, once its `execve` has
    /// finished.
    ///
    /// `Command::spawn` returns when the child's close-on-exec pipe closes,
    /// which happens early in `execve` (`begin_new_exec`), before the kernel
    /// has laid out the new program's argument and environment blocks. Read in
    /// that window, `/proc/<pid>/exe` already names the new binary while
    /// `cmdline` and `environ` are still empty — measured: a TUI captured that
    /// way was later refused its signal because its command line "changed"
    /// from `[]`. So wait, briefly and boundedly, until `exe` is the binary
    /// launched and both blocks are populated, and read the identity then.
    pub fn capture_spawned(pid: i32, launched: &Path) -> Result<Self, String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(id) = Self::capture(pid)
                && id.exe == launched
                && !id.cmdline.is_empty()
                && !id.environ.is_empty()
            {
                return Ok(id);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "pid {pid} never finished exec'ing {} within 5s (last capture: {:?})",
                    launched.display(),
                    Self::capture(pid).map(|id| id.summary())
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Re-read every field and require each one unchanged.
    pub fn verify(&self) -> Result<(), Mismatch> {
        let pid = self.pid;
        if !is_alive(pid) || state(pid) == Some('Z') {
            return Err(Mismatch::Gone);
        }
        let changed = |field: &'static str, detail: String| Mismatch::Changed { field, detail };
        match start_time(pid) {
            Some(t) if t == self.start_time => {}
            Some(t) => {
                return Err(changed(
                    "start time",
                    format!("{t}, recorded {} — the pid was recycled", self.start_time),
                ));
            }
            None => return Err(Mismatch::Gone),
        }
        match exe_path(pid) {
            Some(p) if p == self.exe => {}
            other => {
                return Err(changed(
                    "exe",
                    format!("{other:?}, recorded {:?}", self.exe),
                ));
            }
        }
        match cmdline(pid) {
            Some(c) if c == self.cmdline => {}
            other => {
                return Err(changed(
                    "cmdline",
                    format!("{other:?}, recorded {:?}", self.cmdline),
                ));
            }
        }
        match cwd(pid) {
            Some(c) if c == self.cwd => {}
            other => {
                return Err(changed(
                    "cwd",
                    format!("{other:?}, recorded {:?}", self.cwd),
                ));
            }
        }
        match environ(pid) {
            Some(e) if e == self.environ => {}
            Some(e) => {
                // Name the keys that differ; never print values, which is
                // where a leaked credential would be.
                let differing: BTreeSet<&String> = e
                    .keys()
                    .chain(self.environ.keys())
                    .filter(|k| e.get(*k) != self.environ.get(*k))
                    .collect();
                return Err(changed("environ", format!("differing keys {differing:?}")));
            }
            None => return Err(changed("environ", "unreadable".to_string())),
        }
        match namespace(&pid.to_string(), "mnt") {
            Some(n) if n == self.mnt_ns => {}
            other => {
                return Err(changed(
                    "mount namespace",
                    format!("{other:?}, recorded {}", self.mnt_ns),
                ));
            }
        }
        Ok(())
    }

    /// One line for the evidence file.
    pub fn summary(&self) -> String {
        format!(
            "pid {} · start time {} · exe `{}` · cmdline {:?} · cwd `{}` · mnt {} · {} environment entries",
            self.pid,
            self.start_time,
            self.exe.display(),
            self.cmdline,
            self.cwd.display(),
            self.mnt_ns,
            self.environ.len()
        )
    }
}

/// A process this harness spawned, remembered by its full [`Identity`] and by
/// its un-reaped `Child` handle.
pub struct SandboxProcess {
    pub identity: Identity,
    pub label: String,
    child: Option<std::process::Child>,
}

/// What [`SandboxProcess::terminate`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Terminated {
    /// One SIGTERM was enough.
    Graceful,
    /// It outlived `grace`, its identity was re-verified, and it got SIGKILL.
    Killed,
}

impl SandboxProcess {
    /// Capture `child`'s identity right after spawn, once its `execve` has
    /// finished (see [`Identity::capture_spawned`]).
    pub fn adopt(
        child: std::process::Child,
        launched: &Path,
        label: String,
    ) -> Result<Self, String> {
        let pid = child.id() as i32;
        let identity =
            Identity::capture_spawned(pid, launched).map_err(|e| format!("{label}: {e}"))?;
        Ok(Self {
            identity,
            label,
            child: Some(child),
        })
    }

    pub fn pid(&self) -> i32 {
        self.identity.pid
    }

    /// Whether the process has exited, reaping it if so.
    pub fn has_exited(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => !matches!(c.try_wait(), Ok(None)),
            None => true,
        }
    }

    /// SIGTERM once, wait up to `grace`, and SIGKILL only after re-verifying the
    /// whole identity. Refuses without signalling when verification fails.
    ///
    /// Exactly one SIGTERM, deliberately: the daemon treats a SECOND one as a
    /// forced exit that skips the rest of its graceful teardown.
    pub fn terminate(&mut self, grace: Duration) -> Result<Terminated, Mismatch> {
        self.identity.verify()?;
        // SAFETY: a positive pid whose every identity field was re-read and
        // matched one syscall ago, and whose Child handle is not yet reaped.
        unsafe { libc::kill(self.identity.pid, libc::SIGTERM) };
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.has_exited() {
                return Ok(Terminated::Graceful);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.identity.verify()?;
        // SAFETY: re-verified immediately above; still un-reaped.
        unsafe { libc::kill(self.identity.pid, libc::SIGKILL) };
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        Ok(Terminated::Killed)
    }
}

impl Drop for SandboxProcess {
    fn drop(&mut self) {
        // Reap it if it has exited; never signal from a destructor.
        if let Some(c) = self.child.as_mut() {
            let _ = c.try_wait();
        }
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
        // Match the path as a whole field, not as a substring: one sandbox
        // socket path can be a prefix of another's.
        if !line.split_whitespace().any(|f| f == want.as_ref()) {
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

/// One listening, path-bound Unix socket from the kernel's table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnixListener {
    pub inode: u64,
    pub path: String,
}

/// `__SO_ACCEPTCON` in the `Flags` column of `/proc/net/unix`: the socket is
/// listening.
const SO_ACCEPTCON: u32 = 0x0001_0000;

/// Parse `/proc/net/unix` into its listening, path-bound entries.
///
/// Columns: `Num RefCount Protocol Flags Type St Inode [Path]`. Abstract
/// sockets (`@…`) and unnamed ones are skipped: no deck endpoint is either.
pub fn parse_unix_listeners(table: &str) -> Vec<UnixListener> {
    let mut out = Vec::new();
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let Ok(flags) = u32::from_str_radix(fields[3], 16) else {
            continue;
        };
        if flags & SO_ACCEPTCON == 0 {
            continue;
        }
        let Ok(inode) = fields[6].parse::<u64>() else {
            continue;
        };
        let path = fields[7..].join(" ");
        if path.starts_with('@') {
            continue;
        }
        out.push(UnixListener { inode, path });
    }
    out
}

/// The caller's network namespace's listening Unix sockets. Inside the run's
/// private network namespace that is the sandbox's listeners and nothing else;
/// on the host it is every host listener and none of the sandbox's.
pub fn unix_listeners() -> Result<Vec<UnixListener>, String> {
    let table = std::fs::read_to_string("/proc/net/unix")
        .map_err(|e| format!("read /proc/net/unix: {e}"))?;
    Ok(parse_unix_listeners(&table))
}

/// The socket inodes `pid` holds open, from its `/proc/<pid>/fd` links.
pub fn socket_inodes(pid: i32) -> Result<BTreeSet<u64>, String> {
    let dir = format!("/proc/{pid}/fd");
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("read {dir}: {e}"))? {
        let Ok(entry) = entry else { continue };
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let t = target.to_string_lossy();
        if let Some(inode) = t
            .strip_prefix("socket:[")
            .and_then(|r| r.strip_suffix(']'))
            .and_then(|n| n.parse::<u64>().ok())
        {
            out.insert(inode);
        }
    }
    Ok(out)
}

/// Every visible process holding `inode` open. Processes whose fd table is not
/// readable (another uid) are skipped: they cannot hold one of this uid's deck
/// sockets, which the endpoint directories' `0700` guarantees.
pub fn owners_of(inode: u64) -> Result<Vec<i32>, String> {
    let mut out = Vec::new();
    for pid in pids()? {
        if socket_inodes(pid).is_ok_and(|s| s.contains(&inode)) {
            out.push(pid);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_names_itself() {
        let me = std::process::id() as i32;
        assert!(is_alive(me));
        assert!(cmdline(me).is_some_and(|c| !c.is_empty()));
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
        assert!(!is_alive(0), "pid 0 must never be probed as a process");
    }

    #[test]
    fn the_start_time_is_field_22_even_when_comm_has_spaces_and_parens() {
        // Fields 3..=22 after a hostile comm; starttime (22) is 424242.
        let stat =
            "1234 (a b) (c)) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 424242 1000 10";
        assert_eq!(parse_start_time(stat), Some(424242));
        assert_eq!(parse_state(stat), Some('S'));
    }

    #[test]
    fn a_captured_identity_verifies_against_itself() {
        let me = std::process::id() as i32;
        let id = Identity::capture(me).expect("capture self");
        assert_eq!(id.verify(), Ok(()));
        assert!(id.summary().contains(&format!("pid {me}")));
    }

    #[test]
    fn a_just_spawned_child_is_captured_only_after_its_exec_settles() {
        let bin = std::fs::canonicalize("/bin/sleep").expect("sleep(1)");
        let mut child = std::process::Command::new(&bin)
            .arg("30")
            .env_clear()
            .env("XVER_PROBE", "1")
            .spawn()
            .expect("spawn sleep");
        let id = Identity::capture_spawned(child.id() as i32, &bin);
        // Stop it through its own un-reaped handle before asserting anything.
        let _ = child.kill();
        let _ = child.wait();
        let id = id.expect("capture after exec");
        assert_eq!(id.exe, bin);
        assert_eq!(
            id.cmdline,
            vec![bin.display().to_string(), "30".to_string()]
        );
        assert_eq!(id.environ.get("XVER_PROBE").map(String::as_str), Some("1"));
    }

    #[test]
    fn every_identity_field_is_load_bearing() {
        let me = std::process::id() as i32;
        let base = Identity::capture(me).expect("capture self");
        let mut t = base.clone();
        t.start_time += 1;
        assert!(matches!(
            t.verify(),
            Err(Mismatch::Changed {
                field: "start time",
                ..
            })
        ));
        let mut t = base.clone();
        t.exe = PathBuf::from("/definitely/not/this/exe");
        assert!(matches!(
            t.verify(),
            Err(Mismatch::Changed { field: "exe", .. })
        ));
        let mut t = base.clone();
        t.cmdline.push("daemon".to_string());
        assert!(matches!(
            t.verify(),
            Err(Mismatch::Changed {
                field: "cmdline",
                ..
            })
        ));
        let mut t = base.clone();
        t.cwd = PathBuf::from("/definitely/not/this/cwd");
        assert!(matches!(
            t.verify(),
            Err(Mismatch::Changed { field: "cwd", .. })
        ));
        let mut t = base.clone();
        t.environ
            .insert("DAD_XVER_SANDBOX".into(), "/not/this/run".into());
        assert!(matches!(
            t.verify(),
            Err(Mismatch::Changed {
                field: "environ",
                ..
            })
        ));
        let mut t = base;
        t.mnt_ns = "mnt:[1]".to_string();
        assert!(matches!(
            t.verify(),
            Err(Mismatch::Changed {
                field: "mount namespace",
                ..
            })
        ));
    }

    #[test]
    fn the_unix_table_parser_keeps_only_listening_path_bound_sockets() {
        let table = "Num       RefCount Protocol Flags    Type St Inode Path\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 291707199 /run/user/1000/dot-agent-deck.sock\n\
            0000000000000000: 00000003 00000000 00000000 0001 03 291707300 /run/user/1000/dot-agent-deck.sock\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 1234 @/tmp/.X11-unix/X0\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 5678\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 9999 /tmp/dir with space/a.sock\n";
        let got = parse_unix_listeners(table);
        assert_eq!(
            got,
            vec![
                UnixListener {
                    inode: 291707199,
                    path: "/run/user/1000/dot-agent-deck.sock".into()
                },
                UnixListener {
                    inode: 9999,
                    path: "/tmp/dir with space/a.sock".into()
                },
            ]
        );
    }
}
