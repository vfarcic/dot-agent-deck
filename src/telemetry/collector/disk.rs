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
/// holding an entry named `.git` (a directory for a main checkout, a file for
/// a linked worktree). `None` when no ancestor has one.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| fs::symlink_metadata(candidate.join(".git")).is_ok())
        .map(Path::to_path_buf)
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
/// `O_NOFOLLOW` where `openat2` is unavailable), and what is checked is the
/// directory it opened. So a directory renamed and replaced with a symlink
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
        let Some(name) = top.next_name() else {
            stack.pop();
            continue;
        };
        entries += 1;
        if entries > limits.budget.max_entries
            || limits.cancel.load(Ordering::Relaxed)
            || Instant::now() >= limits.deadline
        {
            complete = false;
            break;
        }
        let Ok(st) = top.stat_at(&name) else {
            continue;
        };
        if !fd::is_dir(&st) {
            if fd::links(&st) > 1 {
                if seen.len() >= limits.budget.max_tracked {
                    complete = false;
                    break;
                }
                if !seen.insert(fd::id(&st)) {
                    continue;
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
        let child = match top.open_child(&name) {
            Ok(child) => child,
            Err(e) => {
                // A directory it may not enter still holds its own blocks; a
                // symlink, a mount point or a vanished entry holds none here.
                let denied = matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM));
                if denied && fd::dev(&st) == device {
                    if seen.len() >= limits.budget.max_tracked {
                        complete = false;
                        break;
                    }
                    if seen.insert(fd::id(&st)) {
                        total += fd::bytes(&st);
                    }
                }
                continue;
            }
        };
        // What was opened is what is checked, not what the name showed.
        let Ok(opened) = child.stat() else {
            continue;
        };
        if !fd::is_dir(&opened) || fd::dev(&opened) != device || child.holds(c".git") {
            continue;
        }
        if seen.len() >= limits.budget.max_tracked {
            complete = false;
            break;
        }
        if !seen.insert(fd::id(&opened)) {
            continue;
        }
        total += fd::bytes(&opened);
        stack.push(child);
    }
    Ok(Walk {
        bytes: total,
        complete,
    })
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
        /// following a symlink (and, on Linux, without crossing a mount).
        pub fn open_child(&self, name: &CStr) -> io::Result<Dir> {
            #[cfg(target_os = "linux")]
            if let Some(opened) = openat2(
                self.fd.0,
                name,
                OPEN_DIR,
                RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH | RESOLVE_NO_XDEV,
            ) {
                return Dir::from_fd(opened?);
            }
            // SAFETY: `name` is a NUL-terminated string and `fd` is open.
            let fd = check(unsafe { libc::openat(self.fd.0, name.as_ptr(), OPEN_DIR) })?;
            Dir::from_fd(fd)
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

        /// Whether the directory holds an entry `name` of any kind.
        pub fn holds(&self, name: &CStr) -> bool {
            self.stat_at(name).is_ok()
        }

        /// The next entry's name, `.` and `..` left out; `None` at the end
        /// or on a read error.
        pub fn next_name(&mut self) -> Option<CString> {
            loop {
                // SAFETY: `stream` is open; the entry it returns is valid
                // until the next `readdir` on it, and is copied before that.
                let entry = unsafe { libc::readdir(self.stream) };
                if entry.is_null() {
                    return None;
                }
                // SAFETY: `d_name` is a NUL-terminated array in the entry.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                if name.to_bytes() != b"." && name.to_bytes() != b".." {
                    return Some(name.to_owned());
                }
            }
        }
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
}
