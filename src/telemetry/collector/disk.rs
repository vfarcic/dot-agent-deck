//! Worktrees: which one a directory is in, the pseudonymous id the collector
//! sends for it, and how much disk it holds.
//!
//! Several runs normally share a worktree, so disk is reported per worktree,
//! never per run. A path is never sent unless the user opts in: the id is a
//! SHA-256 of the canonical path, which `dot-agent-deck-collector worktree-id`
//! computes for a path the user names.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// The git toplevel of `dir`: the nearest directory, `dir` itself included,
/// holding a `.git` that git would take for a checkout's ([`is_git_entry`]).
/// `None` when no ancestor has one.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| is_git_entry(&candidate.join(".git")))
        .map(Path::to_path_buf)
}

/// What a `.git` entry must start with when it is a file: a linked worktree's
/// or a submodule's pointer to its git directory.
const GITDIR_PREFIX: &[u8] = b"gitdir: ";

/// Whether `entry` is a checkout's `.git`: a directory (a main checkout), or
/// a regular file whose contents start with `gitdir: ` (a linked worktree or
/// a submodule). A symlink, wherever it points, and any other kind of entry
/// are not. The file is opened without following a symlink and without
/// waiting on a FIFO swapped in, and what is checked is the file opened.
pub fn is_git_entry(entry: &Path) -> bool {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(meta) = fs::symlink_metadata(entry) else {
        return false;
    };
    if meta.file_type().is_dir() {
        return true;
    }
    if !meta.file_type().is_file() {
        return false;
    }
    let Ok(mut file) = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(entry)
    else {
        return false;
    };
    let mut head = [0u8; GITDIR_PREFIX.len()];
    file.metadata().is_ok_and(|m| m.file_type().is_file())
        && file.read_exact(&mut head).is_ok()
        && head == GITDIR_PREFIX
}

/// `deck.worktree.id`: the lowercase hex of the first 16 bytes of SHA-256
/// over the path's bytes. The path is expected canonical
/// ([`canonical_toplevel`]), so one worktree has one id.
pub fn worktree_id(canonical: &Path) -> String {
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The canonical git toplevel of `path`, which must exist.
pub fn canonical_toplevel(path: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    let dir = if canonical.is_dir() {
        canonical.as_path()
    } else {
        canonical.parent().unwrap_or(&canonical)
    };
    git_toplevel(dir).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is inside no git worktree", path.display()),
        )
    })
}

/// What one walk may spend. A walk that reaches a limit stops there and is
/// reported as incomplete ([`Walk::complete`]), never retried at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// Directory entries looked at, at most.
    pub max_entries: u64,
    /// Inodes held so that a directory or a hard-linked file is counted once,
    /// at most: what bounds the walk's memory.
    pub max_tracked: usize,
    /// How many directories deep below the root it goes, at most. Each level
    /// holds one open directory.
    pub max_depth: usize,
    /// How long one walk may take.
    pub time: Duration,
}

/// The budget of every walk the collector makes. The largest worktree
/// measured for PRD #1859 held 117,437 entries and took 27 s cold.
pub const DEFAULT_BUDGET: Budget = Budget {
    max_entries: 4_000_000,
    max_tracked: 1_000_000,
    max_depth: 64,
    time: Duration::from_secs(600),
};

/// The limits one walk runs under: its budget, when it must stop, and a
/// switch that stops it early.
pub struct Limits<'a> {
    pub budget: &'a Budget,
    pub deadline: Instant,
    pub cancel: &'a AtomicBool,
}

/// What one walk counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Walk {
    pub bytes: u64,
    /// `false` when a limit of its [`Budget`], its deadline or its
    /// cancellation stopped it, or a directory deeper than `max_depth` was
    /// left out: `bytes` is then what it counted, a lower bound.
    pub complete: bool,
}

/// The disk `root` holds, under [`DEFAULT_BUDGET`]: [`walk`]'s `bytes`.
pub fn disk_usage(root: &Path) -> io::Result<u64> {
    let cancel = AtomicBool::new(false);
    let limits = Limits {
        budget: &DEFAULT_BUDGET,
        deadline: Instant::now() + DEFAULT_BUDGET.time,
        cancel: &cancel,
    };
    walk(root, &limits).map(|w| w.bytes)
}

/// The disk `root` holds: `st_blocks × 512` (allocated bytes, which is what
/// fills a disk) over `root` and everything under it, directories included,
/// with each inode counted once. Fails only when `root` itself cannot be
/// opened, which includes `root` being reached through a symlink, in its last
/// component or any other: it is expected canonical ([`canonical_toplevel`]).
///
/// The walk goes through open directories rather than paths: the root is
/// opened refusing a symlink in any component (on Linux with `openat2`,
/// falling back to opening it one component at a time, each relative to the
/// directory opened before it, with `O_NOFOLLOW`, and on Linux each ancestor
/// with `O_PATH`, so that it needs search permission and not read
/// permission, as an ordinary path lookup does), and each
/// subdirectory is opened relative to its parent's open directory without
/// following a symlink (on Linux with `openat2`, which also
/// refuses to cross a mount point, falling back to `openat` with
/// `O_NOFOLLOW` where `openat2` is unavailable, which enters a directory
/// only when its mount id shows it on the root's mount, and leaves one
/// whose mount it cannot tell out of an incomplete figure), and what is
/// checked is the directory it opened. So a directory renamed and replaced with a symlink
/// while the walk runs is not followed. It does not enter a directory on
/// another filesystem than `root`'s: directories' devices are compared, and
/// files' are not, because on overlayfs a file reports the underlying
/// filesystem's device rather than the overlay's its directory reports (so a
/// file bind-mounted inside the worktree is counted, wherever it is from). It
/// skips what it cannot read, and skips — neither counts nor enters — a
/// subdirectory that holds its own `.git` (a nested repository or worktree,
/// reported on its own when a run uses it).
pub fn walk(root: &Path, limits: &Limits<'_>) -> io::Result<Walk> {
    before_open(root.as_os_str());
    let root_dir = match fd::Dir::open_root(root) {
        Ok(dir) => dir,
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {
            // A file as `root`: what it holds itself, looked at through its
            // parent opened the same way, so no component is followed. A
            // symlink is refused here too (`O_NOFOLLOW` with `O_DIRECTORY`
            // reports one as not a directory).
            let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
                return Err(e);
            };
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            let name = std::ffi::CString::new(name.as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            let st = fd::stat_beneath(parent, &name)?;
            if fd::is_dir(&st) || fd::is_symlink(&st) {
                return Err(e);
            }
            return Ok(Walk {
                bytes: fd::bytes(&st),
                complete: true,
            });
        }
        Err(e) => return Err(e),
    };
    let st = root_dir.stat()?;
    let device = fd::dev(&st);
    // The root's mount, read only once a directory is opened by the
    // fallback, which does not refuse to cross one itself.
    let mut root_mount: Option<Option<u64>> = None;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    seen.insert(fd::id(&st));
    let mut total = fd::bytes(&st);
    let mut complete = true;
    let mut entries = 0u64;
    let mut stack = vec![root_dir];
    loop {
        let depth = stack.len();
        let Some(top) = stack.last_mut() else {
            break;
        };
        let name = match top.next_name() {
            Ok(Some(name)) => name,
            Ok(None) => {
                stack.pop();
                continue;
            }
            // A directory that could not be read to its end: what it holds
            // past the error is not counted.
            Err(_) => {
                complete = false;
                stack.pop();
                continue;
            }
        };
        entries += 1;
        if entries > limits.budget.max_entries
            || limits.cancel.load(Ordering::Relaxed)
            || Instant::now() >= limits.deadline
        {
            complete = false;
            break;
        }
        let st = match top.stat_at(&name) {
            Ok(st) => st,
            // Gone since it was listed: nothing of it is left to count.
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => continue,
            // There, but not looked at (a directory it may read and not
            // search): its blocks are not counted.
            Err(_) => {
                complete = false;
                continue;
            }
        };
        if !fd::is_dir(&st) {
            if fd::links(&st) > 1 {
                match track(&mut seen, fd::id(&st), limits.budget.max_tracked) {
                    Tracked::New => {}
                    Tracked::Counted => continue,
                    Tracked::NoRoom => {
                        complete = false;
                        break;
                    }
                }
            }
            total += fd::bytes(&st);
            continue;
        }
        if depth > limits.budget.max_depth {
            complete = false;
            continue;
        }
        before_open(OsStr::from_bytes(name.to_bytes()));
        let (child, opened_by) = match top.open_child(&name) {
            Ok(child) => child,
            Err(e) => {
                match e.raw_os_error() {
                    // A directory it may not enter still holds its own
                    // blocks; on another filesystem it is not the worktree's.
                    Some(libc::EACCES | libc::EPERM) if fd::dev(&st) == device => {
                        match track(&mut seen, fd::id(&st), limits.budget.max_tracked) {
                            Tracked::New => total += fd::bytes(&st),
                            Tracked::Counted => {}
                            Tracked::NoRoom => {
                                complete = false;
                                break;
                            }
                        }
                    }
                    // A symlink or a mount point, which the walk leaves out
                    // on purpose, or an entry that vanished.
                    Some(
                        libc::EACCES
                        | libc::EPERM
                        | libc::ELOOP
                        | libc::ENOTDIR
                        | libc::EXDEV
                        | libc::ENOENT,
                    ) => {}
                    // Any other failure leaves a directory uncounted.
                    _ => complete = false,
                }
                continue;
            }
        };
        // What was opened is what is checked, not what the name showed.
        let Ok(opened) = child.stat() else {
            complete = false;
            continue;
        };
        if !fd::is_dir(&opened) || fd::dev(&opened) != device || child.holds_git() {
            continue;
        }
        if opened_by == fd::OpenedBy::Fallback {
            let root = *root_mount.get_or_insert_with(|| stack[0].mount_id());
            match boundary(root, child.mount_id()) {
                Boundary::Same => {}
                // Another mount, which the walk leaves out on purpose, as
                // `openat2` does on the primary path.
                Boundary::Crossed => continue,
                // Not known to be on the root's mount: it is not entered,
                // and what it holds is missing from the figure.
                Boundary::Unknown => {
                    complete = false;
                    continue;
                }
            }
        }
        match track(&mut seen, fd::id(&opened), limits.budget.max_tracked) {
            Tracked::New => {}
            Tracked::Counted => continue,
            Tracked::NoRoom => {
                complete = false;
                break;
            }
        }
        total += fd::bytes(&opened);
        stack.push(child);
    }
    Ok(Walk {
        bytes: total,
        complete,
    })
}

/// What [`track`] found.
#[derive(Debug, PartialEq, Eq)]
enum Tracked {
    /// Not seen before in this walk, and now held: count it.
    New,
    /// Already counted in this walk: skipping it needs no room.
    Counted,
    /// Not seen before, and the budget holds no more: the walk stops.
    NoRoom,
}

/// Hold `id` in `seen`, at most `max` of them. An inode already held is
/// [`Tracked::Counted`] whatever room is left, so a repeat hard link never
/// stops a walk.
fn track(seen: &mut HashSet<(u64, u64)>, id: (u64, u64), max: usize) -> Tracked {
    if seen.contains(&id) {
        Tracked::Counted
    } else if seen.len() >= max {
        Tracked::NoRoom
    } else {
        seen.insert(id);
        Tracked::New
    }
}

/// Where a directory opened by the `openat` fallback is against the walk's
/// root's mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    /// On the root's mount: walked.
    Same,
    /// On another mount: left out, and the walk stays complete, as on the
    /// primary path, whose `openat2` refuses to open it.
    Crossed,
    /// Not known: left out, and the walk is incomplete. An unknown
    /// boundary is never taken for the root's own mount.
    Unknown,
}

/// The [`Boundary`] of a directory opened by the `openat` fallback, from its
/// mount id and the root's ([`fd::Dir::mount_id`]): the check `openat2`'s
/// `RESOLVE_NO_XDEV` makes on the primary path, which also catches a bind
/// mount of the root's own filesystem, where the devices match.
fn boundary(root: Option<u64>, child: Option<u64>) -> Boundary {
    match (root, child) {
        (Some(root), Some(child)) if root == child => Boundary::Same,
        (Some(_), Some(_)) => Boundary::Crossed,
        _ => Boundary::Unknown,
    }
}

/// The mount id in the text of `/proc/<pid>/fdinfo/<fd>`: its `mnt_id:`
/// line, which Linux writes since 3.15.
#[cfg(any(target_os = "linux", test))]
fn parse_fdinfo_mount_id(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
type OpenHook = Box<dyn FnMut(&OsStr)>;

#[cfg(test)]
thread_local! {
    /// A test's hook, run with the name of each directory just before the
    /// walk opens it (the whole path for the root), to change the tree in the
    /// window between looking at an entry and opening it.
    static BEFORE_OPEN: std::cell::RefCell<Option<OpenHook>> =
        const { std::cell::RefCell::new(None) };
}

fn before_open(_name: &OsStr) {
    #[cfg(test)]
    BEFORE_OPEN.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(_name);
        }
    });
}

/// Open directories and `*at` calls over them.
mod fd {
    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    const OPEN_DIR: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    /// A directory opened only to resolve names beneath it, which needs
    /// search permission on it but not read permission (Linux's `O_PATH`).
    #[cfg(target_os = "linux")]
    const OPEN_PATH: libc::c_int =
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    /// Elsewhere a directory is opened for reading, as the walk's own are.
    #[cfg(not(target_os = "linux"))]
    const OPEN_PATH: libc::c_int = OPEN_DIR;

    struct Fd(libc::c_int);

    impl Fd {
        /// The descriptor, which the caller now owns.
        fn into_raw(self) -> libc::c_int {
            let fd = self.0;
            std::mem::forget(self);
            fd
        }
    }

    impl Drop for Fd {
        fn drop(&mut self) {
            // SAFETY: the descriptor is owned by this value and closed once.
            unsafe { libc::close(self.0) };
        }
    }

    /// An open directory: a descriptor for the `*at` calls, and a stream
    /// over a duplicate of it for its entries.
    pub struct Dir {
        stream: *mut libc::DIR,
        fd: Fd,
    }

    // SAFETY: the stream is owned by this value and used by one thread at a
    // time; nothing else holds the pointer.
    unsafe impl Send for Dir {}

    impl Drop for Dir {
        fn drop(&mut self) {
            // SAFETY: `stream` came from `fdopendir` and is closed once.
            unsafe { libc::closedir(self.stream) };
        }
    }

    fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }

    impl Dir {
        fn from_fd(fd: libc::c_int) -> io::Result<Dir> {
            let fd = Fd(fd);
            // SAFETY: `fd` is an open descriptor.
            let dup = Fd(check(unsafe {
                libc::fcntl(fd.0, libc::F_DUPFD_CLOEXEC, 0)
            })?);
            // SAFETY: `dup` is an open directory descriptor; on success the
            // stream owns it, so it is not closed here.
            let stream = unsafe { libc::fdopendir(dup.0) };
            if stream.is_null() {
                return Err(io::Error::last_os_error());
            }
            std::mem::forget(dup);
            Ok(Dir { stream, fd })
        }

        /// `root`, opened as a directory, no component of it followed when
        /// it is a symlink.
        pub fn open_root(root: &Path) -> io::Result<Dir> {
            Dir::from_fd(open_beneath(root, OPEN_DIR)?.into_raw())
        }

        /// The subdirectory `name`, opened relative to this one without
        /// following a symlink (and, on Linux with `openat2`, without
        /// crossing a mount), and which of the two opened it.
        pub fn open_child(&self, name: &CStr) -> io::Result<(Dir, OpenedBy)> {
            #[cfg(target_os = "linux")]
            if let Some(opened) = openat2(
                self.fd.0,
                name,
                OPEN_DIR,
                RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH | RESOLVE_NO_XDEV,
            ) {
                return Ok((Dir::from_fd(opened?)?, OpenedBy::Openat2));
            }
            // SAFETY: `name` is a NUL-terminated string and `fd` is open.
            let fd = check(unsafe { libc::openat(self.fd.0, name.as_ptr(), OPEN_DIR) })?;
            Ok((Dir::from_fd(fd)?, OpenedBy::Fallback))
        }

        /// The id of the mount the open directory is on: `statx`'s
        /// `STATX_MNT_ID` (Linux 5.8), else the `mnt_id:` line of
        /// `/proc/self/fdinfo/<fd>` (Linux 3.15). `None` when neither
        /// reports one, such as with `/proc` not mounted.
        #[cfg(target_os = "linux")]
        pub fn mount_id(&self) -> Option<u64> {
            self.statx_mount_id().or_else(|| self.fdinfo_mount_id())
        }

        #[cfg(target_os = "linux")]
        fn fdinfo_mount_id(&self) -> Option<u64> {
            #[cfg(test)]
            if NO_FDINFO_MOUNT_ID.with(std::cell::Cell::get) {
                return None;
            }
            let text = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", self.fd.0)).ok()?;
            super::parse_fdinfo_mount_id(&text)
        }

        #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
        fn statx_mount_id(&self) -> Option<u64> {
            #[cfg(test)]
            if NO_STATX_MOUNT_ID.with(std::cell::Cell::get) {
                return None;
            }
            let mut stx = std::mem::MaybeUninit::<libc::statx>::zeroed();
            // The raw syscall rather than the C library's `statx` wrapper,
            // which glibc gained only in 2.28: a reference to the wrapper
            // fails to link against an older glibc, such as the aarch64
            // release build's.
            // SAFETY: the empty path with `AT_EMPTY_PATH` names the open
            // descriptor itself, and `stx` is a writable `struct statx`.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_statx,
                    self.fd.0,
                    c"".as_ptr(),
                    libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                    STATX_MNT_ID,
                    stx.as_mut_ptr(),
                )
            };
            if rc != 0 {
                return None;
            }
            // SAFETY: `statx` succeeded, so it filled `stx`; it was zeroed
            // before, so a field it did not fill reads as 0.
            let stx = unsafe { stx.assume_init() };
            (stx.stx_mask & STATX_MNT_ID != 0).then_some(stx.stx_mnt_id)
        }

        /// Where the `libc` crate defines no `struct statx`, `/proc` alone
        /// reports it.
        #[cfg(all(target_os = "linux", not(any(target_env = "gnu", target_env = "musl"))))]
        fn statx_mount_id(&self) -> Option<u64> {
            None
        }

        /// Elsewhere no mount id is read: the directory's device stands in
        /// for it, so there a mount of the root's own device is not told
        /// apart.
        #[cfg(not(target_os = "linux"))]
        pub fn mount_id(&self) -> Option<u64> {
            self.stat().ok().map(|st| dev(&st))
        }

        /// The open directory's own metadata.
        pub fn stat(&self) -> io::Result<libc::stat> {
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `st` is writable and `fd` is open.
            check(unsafe { libc::fstat(self.fd.0, st.as_mut_ptr()) })?;
            // SAFETY: `fstat` succeeded, so it filled `st`.
            Ok(unsafe { st.assume_init() })
        }

        /// The metadata of the entry `name`, a symlink not followed.
        pub fn stat_at(&self, name: &CStr) -> io::Result<libc::stat> {
            stat_at(&self.fd, name)
        }

        /// Whether the directory holds a checkout's `.git`, by the rule of
        /// [`super::is_git_entry`]: a directory, or a regular file starting
        /// `gitdir: `, opened relative to this one without following a
        /// symlink or waiting on a FIFO.
        pub fn holds_git(&self) -> bool {
            let Ok(st) = self.stat_at(c".git") else {
                return false;
            };
            if is_dir(&st) {
                return true;
            }
            if st.st_mode & libc::S_IFMT != libc::S_IFREG {
                return false;
            }
            let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
            // SAFETY: the name is NUL-terminated and `fd` is open.
            let Ok(file) = check(unsafe { libc::openat(self.fd.0, c".git".as_ptr(), flags) })
            else {
                return false;
            };
            let file = Fd(file);
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `st` is writable and `file` is open.
            if check(unsafe { libc::fstat(file.0, st.as_mut_ptr()) }).is_err() {
                return false;
            }
            // SAFETY: `fstat` succeeded, so it filled `st`.
            if unsafe { st.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
                return false;
            }
            let mut head = [0u8; super::GITDIR_PREFIX.len()];
            let mut read = 0;
            while read < head.len() {
                // SAFETY: the buffer's tail is writable for its length.
                let n = unsafe {
                    libc::read(file.0, head[read..].as_mut_ptr().cast(), head.len() - read)
                };
                if n <= 0 {
                    return false;
                }
                read += n as usize;
            }
            head == super::GITDIR_PREFIX
        }

        /// The next entry's name, `.` and `..` left out; `None` at the
        /// end. A read error is an error, not the end: the entries after it
        /// were not seen.
        pub fn next_name(&mut self) -> io::Result<Option<CString>> {
            loop {
                #[cfg(test)]
                if let Some(errno) = READDIR_ERROR.with(std::cell::Cell::take) {
                    return Err(io::Error::from_raw_os_error(errno));
                }
                // `readdir` tells an error from the end only by `errno`.
                clear_errno();
                // SAFETY: `stream` is open; the entry it returns is valid
                // until the next `readdir` on it, and is copied before that.
                let entry = unsafe { libc::readdir(self.stream) };
                if entry.is_null() {
                    let error = io::Error::last_os_error();
                    return match error.raw_os_error() {
                        None | Some(0) => Ok(None),
                        Some(_) => Err(error),
                    };
                }
                // SAFETY: `d_name` is a NUL-terminated array in the entry.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                if name.to_bytes() != b"." && name.to_bytes() != b".." {
                    return Ok(Some(name.to_owned()));
                }
            }
        }
    }

    /// Which call opened a subdirectory.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OpenedBy {
        /// `openat2`, which refuses to cross a mount. Linux only.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        Openat2,
        /// The `openat` fallback, which does not.
        Fallback,
    }

    /// `STATX_MNT_ID` of `linux/stat.h`.
    #[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
    const STATX_MNT_ID: libc::c_uint = 0x1000;

    /// Set `errno` to 0, so that a call that reports an error only through
    /// it can be told apart from one that succeeded.
    fn clear_errno() {
        // SAFETY: each returns this thread's `errno`, which is writable.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        unsafe {
            *libc::__errno_location() = 0
        };
        // SAFETY: as above.
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        unsafe {
            *libc::__error() = 0
        };
    }

    #[cfg(test)]
    thread_local! {
        /// A test's `errno` for the next `readdir` to fail with, once.
        pub static READDIR_ERROR: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
        /// A test's switch to a kernel whose `statx` reports no mount id.
        pub static NO_STATX_MOUNT_ID: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// A test's switch to a `/proc` whose fdinfo reports no mount id.
        pub static NO_FDINFO_MOUNT_ID: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// The metadata of the entry `name` of the open directory `dir`, a
    /// symlink not followed. `dir` may be an `O_PATH` descriptor.
    fn stat_at(dir: &Fd, name: &CStr) -> io::Result<libc::stat> {
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` is NUL-terminated, `st` writable, `dir` open.
        check(unsafe {
            libc::fstatat(
                dir.0,
                name.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        // SAFETY: `fstatat` succeeded, so it filled `st`.
        Ok(unsafe { st.assume_init() })
    }

    /// The metadata of the entry `name` in the directory `parent`, opened
    /// as [`open_beneath`] does, needing only search permission on it.
    pub fn stat_beneath(parent: &Path, name: &CStr) -> io::Result<libc::stat> {
        stat_at(&open_beneath(parent, OPEN_PATH)?, name)
    }

    /// The directory `path`, opened with `flags`, no component of it
    /// followed when it is a symlink. On Linux `openat2` resolves it in one
    /// call; otherwise, or where `openat2` is unavailable, it is opened one
    /// component at a time, each relative to the directory opened before it,
    /// so a symlink anywhere in the path, put there at any time, is refused
    /// rather than followed. Each component before the last is opened with
    /// [`OPEN_PATH`], so that, as with an ordinary path lookup, an ancestor
    /// needs search permission and not read permission on Linux.
    fn open_beneath(path: &Path, flags: libc::c_int) -> io::Result<Fd> {
        #[cfg(target_os = "linux")]
        {
            let whole = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            if let Some(opened) = openat2(libc::AT_FDCWD, &whole, flags, RESOLVE_NO_SYMLINKS) {
                return opened.map(Fd);
            }
        }
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => names.push(
                    CString::new(name.as_bytes())
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
                ),
                // A canonical path has neither.
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(io::Error::from(io::ErrorKind::InvalidInput));
                }
            }
        }
        let start = if path.has_root() { c"/" } else { c"." };
        let flags_at = |i: usize| {
            if i + 1 == names.len() {
                flags
            } else {
                OPEN_PATH
            }
        };
        let first = if names.is_empty() { flags } else { OPEN_PATH };
        // SAFETY: `start` is a NUL-terminated string.
        let mut dir = Fd(check(unsafe { libc::open(start.as_ptr(), first) })?);
        for (i, name) in names.iter().enumerate() {
            // SAFETY: `name` is a NUL-terminated string and `dir` is open.
            dir = Fd(check(unsafe {
                libc::openat(dir.0, name.as_ptr(), flags_at(i))
            })?);
        }
        Ok(dir)
    }

    #[cfg(target_os = "linux")]
    const RESOLVE_NO_XDEV: u64 = 0x01;
    #[cfg(target_os = "linux")]
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    #[cfg(target_os = "linux")]
    const RESOLVE_BENEATH: u64 = 0x08;

    /// Set once `openat2` turned out to be unavailable (a kernel older than
    /// 5.6, or a seccomp filter): every later open uses `openat`.
    #[cfg(target_os = "linux")]
    static NO_OPENAT2: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    #[cfg(all(target_os = "linux", test))]
    thread_local! {
        /// A test's switch to the `openat` fallback.
        pub static FORCE_OPENAT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// `openat2(dirfd, name, flags, resolve)`: `None` when the kernel does
    /// not provide it, for the caller to fall back to `openat`.
    #[cfg(target_os = "linux")]
    fn openat2(
        dirfd: libc::c_int,
        name: &CStr,
        flags: libc::c_int,
        resolve: u64,
    ) -> Option<io::Result<libc::c_int>> {
        use std::sync::atomic::Ordering;
        #[cfg(test)]
        if FORCE_OPENAT.with(std::cell::Cell::get) {
            return None;
        }
        if NO_OPENAT2.load(Ordering::Relaxed) {
            return None;
        }
        /// `struct open_how` of `linux/openat2.h`.
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        let how = OpenHow {
            flags: flags as u64,
            mode: 0,
            resolve,
        };
        // SAFETY: `name` is NUL-terminated and `how` is an `open_how` of the
        // size passed.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dirfd,
                name.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if rc >= 0 {
            return Some(Ok(rc as libc::c_int));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOSYS | libc::EPERM | libc::EINVAL | libc::E2BIG) => {
                NO_OPENAT2.store(true, Ordering::Relaxed);
                None
            }
            _ => Some(Err(error)),
        }
    }

    // The `stat` fields' types differ between platforms; these read them as
    // the walk uses them.

    #[allow(clippy::unnecessary_cast)]
    pub fn dev(st: &libc::stat) -> u64 {
        st.st_dev as u64
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn id(st: &libc::stat) -> (u64, u64) {
        (st.st_dev as u64, st.st_ino as u64)
    }

    #[allow(clippy::unnecessary_cast)]
    pub fn links(st: &libc::stat) -> u64 {
        st.st_nlink as u64
    }

    pub fn bytes(st: &libc::stat) -> u64 {
        u64::try_from(st.st_blocks).unwrap_or(0) * 512
    }

    pub fn is_dir(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_symlink(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFLNK
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    fn usage_of(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().blocks() * 512
    }

    /// The walk expects a canonical root, and a temp directory's path can
    /// hold a symlink.
    fn canonical(tmp: &tempfile::TempDir) -> PathBuf {
        fs::canonicalize(tmp.path()).unwrap()
    }

    fn limits<'a>(budget: &'a Budget, cancel: &'a AtomicBool) -> Limits<'a> {
        Limits {
            budget,
            deadline: Instant::now() + budget.time,
            cancel,
        }
    }

    /// Run `f` with `hook` called before each directory is opened.
    fn with_hook<T>(hook: impl FnMut(&OsStr) + 'static, f: impl FnOnce() -> T) -> T {
        BEFORE_OPEN.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        let out = f();
        BEFORE_OPEN.with(|h| *h.borrow_mut() = None);
        out
    }

    /// Both ways of opening a directory: `openat2` where the kernel has it,
    /// and the `openat` fallback.
    fn each_open_path(mut f: impl FnMut()) {
        f();
        #[cfg(target_os = "linux")]
        {
            fd::FORCE_OPENAT.with(|c| c.set(true));
            f();
            fd::FORCE_OPENAT.with(|c| c.set(false));
        }
    }

    /// Write `len` incompressible bytes and flush them, so the file's
    /// allocated size is settled before anything counts it, and a
    /// compressing filesystem cannot shrink it to almost nothing.
    fn write_allocated(path: &Path, len: usize) {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let mut file = fs::File::create(path).unwrap();
        std::io::Write::write_all(&mut file, &bytes).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn toplevel_is_the_nearest_directory_with_a_git_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        fs::create_dir_all(main.join(".git")).unwrap();
        fs::create_dir_all(main.join("src/deep")).unwrap();
        let linked = tmp.path().join("linked");
        fs::create_dir_all(linked.join("sub")).unwrap();
        fs::write(
            linked.join(".git"),
            "gitdir: ../main/.git/worktrees/linked\n",
        )
        .unwrap();

        assert_eq!(git_toplevel(&main.join("src/deep")), Some(main.clone()));
        assert_eq!(git_toplevel(&main), Some(main.clone()));
        assert_eq!(git_toplevel(&linked.join("sub")), Some(linked.clone()));
        let outside = tmp.path().join("plain");
        fs::create_dir_all(&outside).unwrap();
        // The temp directory's own ancestors hold no `.git` on a sane box;
        // whatever they hold, the answer is not inside `plain`.
        assert!(git_toplevel(&outside).is_none_or(|t| !t.starts_with(&outside)));
    }

    /// Finding 5: only a `.git` directory, or a regular `.git` file that
    /// starts `gitdir: `, makes a checkout. A directory holding a `.git`
    /// symlink, to a real git directory even, or a `.git` file of any other
    /// content, is not one.
    #[test]
    fn a_git_symlink_or_a_stray_git_file_makes_no_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let base = canonical(&tmp);
        let real = base.join("real");
        fs::create_dir_all(real.join(".git")).unwrap();
        let linked = base.join("linked");
        fs::create_dir_all(linked.join("sub")).unwrap();
        std::os::unix::fs::symlink(real.join(".git"), linked.join(".git")).unwrap();
        let stray = base.join("stray");
        fs::create_dir_all(&stray).unwrap();
        fs::write(stray.join(".git"), "not a pointer\n").unwrap();
        let short = base.join("short");
        fs::create_dir_all(&short).unwrap();
        fs::write(short.join(".git"), "gitdir").unwrap();

        assert!(!is_git_entry(&linked.join(".git")));
        assert!(!is_git_entry(&stray.join(".git")));
        assert!(!is_git_entry(&short.join(".git")));
        assert!(is_git_entry(&real.join(".git")));
        for dir in [
            linked.join("sub"),
            linked.clone(),
            stray.clone(),
            short.clone(),
        ] {
            // Whatever the temp directory's ancestors hold, the answer is
            // not one of these.
            assert!(
                git_toplevel(&dir).is_none_or(|t| !t.starts_with(&base)),
                "{dir:?}: {:?}",
                git_toplevel(&dir)
            );
        }
        assert!(
            canonical_toplevel(&linked).is_err()
                || !canonical_toplevel(&linked).unwrap().starts_with(&base)
        );

        // Inside a worktree's walk, such a directory is not a nested
        // checkout: it is walked and counted like any other.
        let root = base.join("wt");
        fs::create_dir_all(root.join(".git")).unwrap();
        let inner = root.join("inner");
        fs::create_dir_all(&inner).unwrap();
        std::os::unix::fs::symlink(real.join(".git"), inner.join(".git")).unwrap();
        write_allocated(&inner.join("data.bin"), 64 * 1024);
        each_open_path(|| {
            let total = disk_usage(&root).unwrap();
            assert!(total >= 64 * 1024, "{total}: the directory was skipped");
        });
    }

    #[test]
    fn worktree_id_is_a_stable_pseudonym() {
        let id = worktree_id(Path::new("/home/u/code/repo"));
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(id, worktree_id(Path::new("/home/u/code/repo")));
        assert_ne!(id, worktree_id(Path::new("/home/u/code/repo2")));
        // The first 16 bytes of SHA-256("/a"), as `printf /a | sha256sum` prints them.
        assert_eq!(
            worktree_id(Path::new("/a")),
            "6a50dc8584134c7de537c0052ff6d236"
        );
    }

    #[test]
    fn canonical_toplevel_resolves_symlinks_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        let expected = fs::canonicalize(&repo).unwrap();
        assert_eq!(canonical_toplevel(&link.join("src")).unwrap(), expected);
        assert_eq!(
            canonical_toplevel(&repo.join("src/lib.rs")).unwrap(),
            expected
        );
        assert!(canonical_toplevel(&tmp.path().join("missing")).is_err());
    }

    #[test]
    fn usage_counts_allocated_bytes_once_per_inode_and_skips_nested_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp).join("wt");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let big = root.join("target/debug/big.bin");
        write_allocated(&big, 256 * 1024);
        fs::hard_link(&big, root.join("target/debug/again.bin")).unwrap();
        std::os::unix::fs::symlink("/", root.join("escape")).unwrap();
        let nested = root.join("nested");
        fs::create_dir_all(nested.join(".git")).unwrap();
        write_allocated(&nested.join("huge.bin"), 512 * 1024);

        let total = disk_usage(&root).unwrap();
        let expected: u64 = [
            root.clone(),
            root.join(".git"),
            root.join("target"),
            root.join("target/debug"),
            big.clone(),
            root.join("escape"),
        ]
        .iter()
        .map(|p| usage_of(p))
        .sum();
        assert_eq!(total, expected);
        assert!(total >= 256 * 1024, "{total}");
        assert!(
            total < 256 * 1024 + 512 * 1024,
            "{total}: nested repo or hard link counted"
        );
        assert_eq!(disk_usage(&nested).unwrap(), {
            let n: u64 = [nested.clone(), nested.join(".git"), nested.join("huge.bin")]
                .iter()
                .map(|p| usage_of(p))
                .sum();
            n
        });
    }

    #[test]
    fn usage_of_a_missing_root_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(disk_usage(&tmp.path().join("gone")).is_err());
    }

    /// A1: a directory that passed the walk's look and is then replaced with
    /// a symlink, between that look and the open, is not followed.
    #[test]
    fn a_directory_swapped_for_a_symlink_is_not_followed() {
        each_open_path(|| {
            let tmp = tempfile::tempdir().unwrap();
            let base = canonical(&tmp);
            let root = base.join("wt");
            fs::create_dir_all(root.join(".git")).unwrap();
            fs::create_dir_all(root.join("sub")).unwrap();
            let outside = base.join("outside");
            fs::create_dir_all(&outside).unwrap();
            write_allocated(&outside.join("secret.bin"), 512 * 1024);
            let (sub, moved, target) = (root.join("sub"), base.join("moved"), outside.clone());
            let mut swapped = false;
            let total = with_hook(
                move |name| {
                    if name == "sub" && !swapped {
                        swapped = true;
                        fs::rename(&sub, &moved).unwrap();
                        std::os::unix::fs::symlink(&target, &sub).unwrap();
                    }
                },
                || disk_usage(&root).unwrap(),
            );
            assert!(
                fs::symlink_metadata(root.join("sub"))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the hook ran"
            );
            assert!(total < 512 * 1024, "{total}: followed the swapped symlink");
        });
    }

    /// A1: a root replaced with a symlink before it is opened fails the walk
    /// rather than measuring where the symlink points.
    #[test]
    fn a_root_swapped_for_a_symlink_is_not_walked() {
        each_open_path(|| {
            let tmp = tempfile::tempdir().unwrap();
            let base = canonical(&tmp);
            let root = base.join("wt");
            fs::create_dir_all(root.join(".git")).unwrap();
            let outside = base.join("outside");
            fs::create_dir_all(&outside).unwrap();
            write_allocated(&outside.join("secret.bin"), 512 * 1024);
            let (swap_root, moved, target) = (root.clone(), base.join("moved"), outside.clone());
            let result = with_hook(
                move |name| {
                    if Path::new(name) == swap_root {
                        fs::rename(&swap_root, &moved).unwrap();
                        std::os::unix::fs::symlink(&target, &swap_root).unwrap();
                    }
                },
                || disk_usage(&root),
            );
            assert!(result.is_err(), "{result:?}");
        });
    }

    /// A1/U1: a root reached through an ancestor that was replaced with a
    /// symlink after the root was found fails the walk, on both open paths,
    /// rather than measuring where the symlink points.
    #[test]
    fn a_root_whose_ancestor_is_swapped_for_a_symlink_is_not_walked() {
        each_open_path(|| {
            let tmp = tempfile::tempdir().unwrap();
            let base = canonical(&tmp);
            let parent = base.join("parent");
            let root = parent.join("wt");
            fs::create_dir_all(root.join(".git")).unwrap();
            let outside = base.join("outside");
            fs::create_dir_all(outside.join("wt/.git")).unwrap();
            write_allocated(&outside.join("wt/secret.bin"), 512 * 1024);
            let (swap, moved, target, whole) = (
                parent.clone(),
                base.join("moved"),
                outside.clone(),
                root.clone(),
            );
            let result = with_hook(
                move |name| {
                    if Path::new(name) == whole {
                        fs::rename(&swap, &moved).unwrap();
                        std::os::unix::fs::symlink(&target, &swap).unwrap();
                    }
                },
                || disk_usage(&root),
            );
            assert!(
                fs::symlink_metadata(&parent)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the hook ran"
            );
            assert!(result.is_err(), "{result:?}");
        });
    }

    /// Whether `result` is the permission denial a mode-restricted fixture
    /// is meant to produce.
    fn denied<T>(result: io::Result<T>) -> bool {
        matches!(result, Err(e) if e.kind() == io::ErrorKind::PermissionDenied)
    }

    /// V2: a root below an ancestor the user may search but not read is
    /// walked on both open paths, as a directory and as a file, as an
    /// ordinary path lookup would reach it.
    #[test]
    fn a_root_below_a_search_only_ancestor_is_walked() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let ancestor = canonical(&tmp).join("search-only");
        let root = ancestor.join("wt");
        fs::create_dir_all(root.join(".git")).unwrap();
        let file = ancestor.join("file.bin");
        write_allocated(&file, 64 * 1024);
        let expected = usage_of(&root) + usage_of(&root.join(".git"));
        let file_bytes = usage_of(&file);
        // 0711 would leave its owner, this test, able to read it.
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o111)).unwrap();
        // Whether the mode denies this process the listing is asked of the
        // directory, not of the uid: a holder of CAP_DAC_OVERRIDE or
        // CAP_DAC_READ_SEARCH may list it anyway, while root without both
        // is refused like any other user.
        if !denied(fs::read_dir(&ancestor)) {
            fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "SKIP: listing a directory of mode 0111 was not refused as denied \
                 (a holder of CAP_DAC_OVERRIDE or CAP_DAC_READ_SEARCH, as root \
                 usually is, may list it), so there is no search-only ancestor \
                 to walk below"
            );
            return;
        }
        let mut results = Vec::new();
        each_open_path(|| results.push((disk_usage(&root), disk_usage(&file))));
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o755)).unwrap();
        for (dir, file) in results {
            assert_eq!(dir.unwrap(), expected);
            assert_eq!(file.unwrap(), file_bytes);
        }
    }

    /// U6: a directory the walk may not enter is held to the same inode
    /// budget as one it enters, and a walk that reaches the budget there is
    /// incomplete.
    #[test]
    fn denied_directories_count_against_the_inode_budget() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp).join("wt");
        fs::create_dir_all(root.join(".git")).unwrap();
        let locked: Vec<PathBuf> = (0..4).map(|i| root.join(format!("locked{i}"))).collect();
        for dir in &locked {
            fs::create_dir(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
        }
        // As in the test above, asked of the directories rather than of the
        // uid.
        if !locked.iter().all(|dir| denied(fs::read_dir(dir))) {
            for dir in &locked {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
            }
            eprintln!(
                "SKIP: listing a directory of mode 0000 was not refused as denied \
                 (a holder of CAP_DAC_OVERRIDE or CAP_DAC_READ_SEARCH, as root \
                 usually is, may list it), so there is no directory the walk may \
                 not enter"
            );
            return;
        }
        let cancel = AtomicBool::new(false);
        let full = walk(&root, &limits(&DEFAULT_BUDGET, &cancel));
        let small = Budget {
            max_tracked: 3,
            ..DEFAULT_BUDGET
        };
        let partial = walk(&root, &limits(&small, &cancel));
        for dir in &locked {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (full, partial) = (full.unwrap(), partial.unwrap());
        assert!(full.complete, "{full:?}");
        // The root, `.git` and the locked directories, each counted.
        assert_eq!(full.bytes, {
            let n: u64 = [root.clone(), root.join(".git")]
                .iter()
                .chain(&locked)
                .map(|p| usage_of(p))
                .sum();
            n
        });
        assert!(!partial.complete, "{partial:?}");
        // A directory holds no blocks on tmpfs, so both can be 0.
        assert!(partial.bytes <= full.bytes, "{partial:?} vs {full:?}");
    }

    /// A2: each limit of a walk's budget stops it, and says so.
    #[test]
    fn a_walk_past_its_budget_is_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp).join("wt");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        for i in 0..10 {
            fs::write(root.join(format!("a/b/f{i}")), "x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let full = walk(&root, &limits(&DEFAULT_BUDGET, &cancel)).unwrap();
        assert!(full.complete);
        for budget in [
            Budget {
                max_entries: 3,
                ..DEFAULT_BUDGET
            },
            Budget {
                max_depth: 1,
                ..DEFAULT_BUDGET
            },
            Budget {
                max_tracked: 2,
                ..DEFAULT_BUDGET
            },
            Budget {
                time: Duration::ZERO,
                ..DEFAULT_BUDGET
            },
        ] {
            let partial = walk(&root, &limits(&budget, &cancel)).unwrap();
            assert!(!partial.complete, "{budget:?}");
            assert!(
                partial.bytes < full.bytes,
                "{budget:?}: {partial:?} vs {full:?}"
            );
        }
        cancel.store(true, Ordering::Relaxed);
        let cancelled = walk(&root, &limits(&DEFAULT_BUDGET, &cancel)).unwrap();
        assert!(!cancelled.complete);
        assert_eq!(cancelled.bytes, usage_of(&root));
    }

    /// Finding 9: once the inode budget is full, a hard link to an inode
    /// already counted is skipped, not taken for one more to hold, so it
    /// does not stop the walk.
    #[test]
    fn repeat_hard_links_never_stop_a_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp).join("wt");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("f.bin");
        write_allocated(&file, 16 * 1024);
        for i in 0..8 {
            fs::hard_link(&file, root.join(format!("link{i}"))).unwrap();
        }
        // The root and the file: exactly the inodes there are to hold.
        let full = Budget {
            max_tracked: 2,
            ..DEFAULT_BUDGET
        };
        let cancel = AtomicBool::new(false);
        let walked = walk(&root, &limits(&full, &cancel)).unwrap();
        assert!(walked.complete, "{walked:?}");
        assert_eq!(walked.bytes, usage_of(&root) + usage_of(&file));
    }

    /// Finding 3: a directory whose listing fails part-way is not read to
    /// its end, so the walk is incomplete rather than a complete figure
    /// short of what that directory holds.
    #[test]
    fn a_readdir_error_makes_the_walk_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp).join("wt");
        fs::create_dir_all(root.join("sub")).unwrap();
        write_allocated(&root.join("sub/f.bin"), 16 * 1024);
        let cancel = AtomicBool::new(false);
        let whole = walk(&root, &limits(&DEFAULT_BUDGET, &cancel)).unwrap();
        assert!(whole.complete);
        fd::READDIR_ERROR.with(|e| e.set(Some(libc::EIO)));
        let failed = walk(&root, &limits(&DEFAULT_BUDGET, &cancel));
        fd::READDIR_ERROR.with(|e| e.set(None));
        let failed = failed.unwrap();
        assert!(!failed.complete, "{failed:?}");
        assert_eq!(failed.bytes, usage_of(&root));
    }

    /// Finding 3: an entry the walk lists but may not look at (in a
    /// directory it may read but not search) is not counted, so the walk is
    /// incomplete; while a directory it may not enter at all is the counted
    /// case, and leaves it complete.
    #[test]
    fn an_entry_it_cannot_look_at_makes_the_walk_incomplete() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp).join("wt");
        let unsearchable = root.join("read-only");
        fs::create_dir_all(&unsearchable).unwrap();
        write_allocated(&unsearchable.join("f.bin"), 16 * 1024);
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o444)).unwrap();
        let looked = fs::symlink_metadata(unsearchable.join("f.bin"));
        if !denied(looked) {
            fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!(
                "SKIP: looking at an entry of a directory of mode 0444 was not refused \
                 as denied (a holder of CAP_DAC_OVERRIDE or CAP_DAC_READ_SEARCH, as \
                 root usually is, may), so there is no entry the walk cannot look at"
            );
            return;
        }
        let cancel = AtomicBool::new(false);
        let mut results = Vec::new();
        each_open_path(|| results.push(walk(&root, &limits(&DEFAULT_BUDGET, &cancel))));
        fs::set_permissions(&unsearchable, fs::Permissions::from_mode(0o755)).unwrap();
        for result in results {
            let walked = result.unwrap();
            assert!(!walked.complete, "{walked:?}");
            assert!(walked.bytes < 16 * 1024, "{walked:?}");
        }
    }

    /// Finding 8: the fallback's mount check. A directory is on the root's
    /// mount, or on another, only when both mount ids are known; otherwise
    /// its boundary is unknown (R8).
    #[test]
    fn a_boundary_is_known_only_when_both_ids_are() {
        assert_eq!(boundary(Some(1), Some(2)), Boundary::Crossed);
        assert_eq!(boundary(Some(1), Some(1)), Boundary::Same);
        assert_eq!(boundary(None, Some(2)), Boundary::Unknown);
        assert_eq!(boundary(Some(1), None), Boundary::Unknown);
        assert_eq!(boundary(None, None), Boundary::Unknown);
    }

    /// R8: the mount id is read from an fdinfo file's `mnt_id:` line.
    #[test]
    fn fdinfo_names_the_mount_id() {
        let fdinfo = "pos:\t0\nflags:\t02304000\nmnt_id:\t29\nino:\t1835009\n";
        assert_eq!(parse_fdinfo_mount_id(fdinfo), Some(29));
        assert_eq!(parse_fdinfo_mount_id("mnt_id: 7"), Some(7));
        assert_eq!(parse_fdinfo_mount_id("pos:\t0\nflags:\t02\n"), None);
        assert_eq!(parse_fdinfo_mount_id("mnt_id:\tx\n"), None);
        assert_eq!(parse_fdinfo_mount_id(""), None);
    }

    /// Run `f` with `statx`'s mount id, fdinfo's, or both, switched off.
    #[cfg(target_os = "linux")]
    fn without_mount_ids<T>(statx: bool, fdinfo: bool, f: impl FnOnce() -> T) -> T {
        fd::NO_STATX_MOUNT_ID.with(|c| c.set(!statx));
        fd::NO_FDINFO_MOUNT_ID.with(|c| c.set(!fdinfo));
        let out = f();
        fd::NO_STATX_MOUNT_ID.with(|c| c.set(false));
        fd::NO_FDINFO_MOUNT_ID.with(|c| c.set(false));
        out
    }

    /// Finding 8 and R8: the mount ids the fallback compares are real, from
    /// `statx` and from fdinfo alike: a directory and its subdirectory share
    /// one, and `/proc`, a mount of its own, has another than `/`'s. Where
    /// `statx` reports none (a kernel older than 5.8), fdinfo still does.
    #[cfg(target_os = "linux")]
    #[test]
    fn mount_ids_tell_mounts_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp);
        fs::create_dir_all(root.join("sub")).unwrap();
        let ids = |statx: bool, fdinfo: bool| {
            without_mount_ids(statx, fdinfo, || {
                let dir = fd::Dir::open_root(&root).unwrap();
                let (sub, _) = dir.open_child(c"sub").unwrap();
                let slash = fd::Dir::open_root(Path::new("/")).unwrap().mount_id();
                let proc = fd::Dir::open_root(Path::new("/proc")).unwrap().mount_id();
                (dir.mount_id(), sub.mount_id(), slash, proc)
            })
        };
        let from_fdinfo = ids(false, true);
        let (dir, sub, slash, proc) = from_fdinfo;
        assert!(dir.is_some(), "fdinfo reported no mount id");
        assert_eq!(sub, dir);
        assert!(slash.is_some() && proc.is_some());
        assert_eq!(boundary(slash, proc), Boundary::Crossed);
        assert_eq!(boundary(dir, sub), Boundary::Same);
        let from_statx = ids(true, false);
        if from_statx.0.is_none() {
            eprintln!("SKIP (statx half only): this kernel's statx reports no mount id");
        } else {
            // Both name the same mounts by the same ids.
            assert_eq!(from_statx, from_fdinfo);
        }
        assert_eq!(ids(false, false), (None, None, None, None));
    }

    /// Finding 8 and R8: on the fallback a tree on one mount measures the
    /// same as on the primary path, whichever source the mount ids come
    /// from; with no mount id at all, no subdirectory is entered and the
    /// walk says it is incomplete rather than guess the boundary.
    #[test]
    fn the_fallback_enters_only_directories_known_to_be_on_the_roots_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp).join("wt");
        fs::create_dir_all(root.join("a/b")).unwrap();
        write_allocated(&root.join("a/b/f.bin"), 16 * 1024);
        let budget = DEFAULT_BUDGET.clone();
        let cancel = AtomicBool::new(false);
        let primary = walk(&root, &limits(&budget, &cancel)).unwrap();
        assert!(primary.complete);
        #[cfg(target_os = "linux")]
        {
            fd::FORCE_OPENAT.with(|c| c.set(true));
            let walked = |statx: bool, fdinfo: bool| {
                without_mount_ids(statx, fdinfo, || {
                    walk(&root, &limits(&budget, &cancel)).unwrap()
                })
            };
            let with_both = walked(true, true);
            let fdinfo_only = walked(false, true);
            let neither = walked(false, false);
            fd::FORCE_OPENAT.with(|c| c.set(false));
            assert_eq!(with_both, primary);
            assert_eq!(fdinfo_only, primary);
            assert!(!neither.complete, "{neither:?}");
            assert!(neither.bytes < primary.bytes, "{neither:?} vs {primary:?}");
        }
    }
}
