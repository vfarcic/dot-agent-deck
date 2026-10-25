//! Directory handles the deck trusts, one verified component at a time.
//!
//! Moved here from `inbox_receipt` (where the receipt spool introduced it) by
//! PRD #1666 part 3, round 5, so the retained-report store can share it. The
//! operations are unchanged except for two additions, [`TrustedDir::sync`] and
//! [`TrustedDir::rename_into`].
//!
//! On Unix every operation is relative to a held, verified directory
//! descriptor, so renaming a parent cannot redirect one. On Windows the checks
//! are path-based and are **not** confinement: see the Windows
//! [`TrustedDir`]'s own documentation.

use std::ffi::OsStr;
use std::io;

/// A path component the deck creates: ASCII letters, digits, `_` and `-`, at
/// most 128 bytes.
pub(crate) fn valid_component(name: &str) -> bool {
    crate::inbox_receipt::valid_component(name)
}

fn refused(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, what.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Dir,
    /// A symlink, FIFO, socket, device or reparse point: never opened.
    Other,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EntryInfo {
    pub(crate) kind: EntryKind,
    pub(crate) len: u64,
}

/// A file name inside one directory: no separator, not `.` or `..`.
pub(crate) fn plain(name: &OsStr) -> io::Result<()> {
    #[cfg(unix)]
    let bad = {
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = name.as_bytes();
        bytes.is_empty()
            || bytes == b"."
            || bytes == b".."
            || bytes.iter().any(|b| matches!(b, b'/' | b'\0'))
    };
    #[cfg(windows)]
    let bad = {
        use std::os::windows::ffi::OsStrExt as _;
        let wide: Vec<u16> = name.encode_wide().collect();
        wide.is_empty()
            || name == OsStr::new(".")
            || name == OsStr::new("..")
            || wide
                .iter()
                .any(|&c| c == u16::from(b'/') || c == u16::from(b'\\') || c == 0)
    };
    if bad {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a plain file name"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) use unix::{DirStream, TrustedDir};
#[cfg(windows)]
pub(crate) use windows::{DirStream, TrustedDir};

/// Set a file this module just created owner-only: the one call
/// `create_new_owner_only` makes after the file exists, with a test seam that
/// fails it.
fn owner_only(file: &std::fs::File) -> io::Result<()> {
    #[cfg(test)]
    if test_seams::take_owner_only_failure() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected owner-only failure",
        ));
    }
    crate::platform::fsperm::set_file_owner_only(file)
}

#[cfg(unix)]
mod unix {
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    use super::{EntryInfo, EntryKind, owner_only, plain};
    use super::{refused, valid_component};

    fn cstr(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a name"))
    }

    fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }

    /// Clear `errno`, so a null `readdir` can tell the end of the
    /// directory from an error.
    fn clear_errno() {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: returns this thread's own `errno` slot, always writable.
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly"
        ))]
        // SAFETY: returns this thread's own `errno` slot, always writable.
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
        // SAFETY: returns this thread's own `errno` slot, always writable.
        unsafe {
            *libc::__errno() = 0;
        }
    }

    fn fstat(fd: RawFd) -> io::Result<libc::stat> {
        let mut st = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fd` is a live descriptor and `st` is writable storage
        // for one `stat`, initialised by a successful call.
        cvt(unsafe { libc::fstat(fd, st.as_mut_ptr()) })?;
        // SAFETY: fstat succeeded and filled it.
        Ok(unsafe { st.assume_init() })
    }

    /// A directory owned by this user; `strict` also refuses any group or
    /// other permission bit.
    fn verify(fd: RawFd, what: &str, strict: bool) -> io::Result<()> {
        let st = fstat(fd)?;
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(refused(format!("{what} is not a directory")));
        }
        // SAFETY: geteuid cannot fail and touches no memory.
        if st.st_uid != unsafe { libc::geteuid() } {
            return Err(refused(format!("{what} is owned by another user")));
        }
        if strict && st.st_mode & 0o077 != 0 {
            return Err(refused(format!(
                "{what} is open to other users (mode {:o})",
                st.st_mode & 0o777
            )));
        }
        Ok(())
    }

    /// A verified directory, held by descriptor: every operation is
    /// relative to it, so a path swapped above or beside it later cannot
    /// redirect one.
    #[derive(Debug)]
    pub(crate) struct TrustedDir {
        fd: OwnedFd,
    }

    impl TrustedDir {
        /// The trust anchor, opened by path: a directory owned by this
        /// user. Its ancestors are trusted as the OS resolves them.
        pub(crate) fn anchor(path: &Path) -> io::Result<Self> {
            let c = cstr(path.as_os_str())?;
            // SAFETY: a valid NUL-terminated path; the descriptor is owned
            // below.
            let fd = cvt(unsafe {
                libc::open(
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            })?;
            // SAFETY: `fd` was just opened and is owned by nobody else.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            verify(fd.as_raw_fd(), &path.display().to_string(), false)?;
            Ok(TrustedDir { fd })
        }

        /// Open the directory `name` below without following a link.
        fn open_dir(&self, name: &OsStr) -> io::Result<OwnedFd> {
            let c = cstr(name)?;
            // SAFETY: a live directory descriptor and a valid name; the
            // descriptor is owned below.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                    return Err(refused(format!("{name:?} is not a plain directory")));
                }
                return Err(error);
            }
            // SAFETY: just opened, owned by nobody else.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }

        /// One component below: created owner-only when `create`, opened
        /// without following a link, and verified owner-only.
        pub(crate) fn child(&self, name: &str, create: bool) -> io::Result<Self> {
            if !valid_component(name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name:?} is not a spool path component"),
                ));
            }
            if create {
                let c = cstr(OsStr::new(name))?;
                // SAFETY: a live directory descriptor and a valid name.
                let rc = unsafe { libc::mkdirat(self.fd.as_raw_fd(), c.as_ptr(), 0o700) };
                if rc < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error);
                    }
                }
            }
            let fd = self.open_dir(OsStr::new(name))?;
            verify(fd.as_raw_fd(), name, true)?;
            Ok(TrustedDir { fd })
        }

        /// A directory below that is about to be removed: opened without
        /// following a link and owned by this user, any mode.
        pub(crate) fn child_for_removal(&self, name: &OsStr) -> io::Result<Self> {
            plain(name)?;
            let fd = self.open_dir(name)?;
            verify(fd.as_raw_fd(), &format!("{name:?}"), false)?;
            Ok(TrustedDir { fd })
        }

        pub(crate) fn try_clone(&self) -> io::Result<Self> {
            Ok(TrustedDir {
                fd: self.fd.try_clone()?,
            })
        }

        /// A new owner-only file, refused when the name exists or is a link.
        /// When its mode cannot be set, the file it just created is removed
        /// again, so a failure leaves nothing at the name.
        pub(crate) fn create_new_owner_only(&self, name: &str) -> io::Result<File> {
            plain(OsStr::new(name))?;
            let c = cstr(OsStr::new(name))?;
            let mode: libc::c_uint = 0o600;
            // SAFETY: a live directory descriptor, a valid name and a mode;
            // the descriptor is owned below.
            let fd = cvt(unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    mode,
                )
            })?;
            // SAFETY: just opened, owned by nobody else.
            let file = unsafe { File::from_raw_fd(fd) };
            if let Err(error) = owner_only(&file) {
                drop(file);
                let _ = self.remove_file(name);
                return Err(error);
            }
            Ok(file)
        }

        pub(crate) fn link(&self, from: &str, to: &str) -> io::Result<()> {
            let (from, to) = (OsStr::new(from), OsStr::new(to));
            plain(from)?;
            plain(to)?;
            let (f, t) = (cstr(from)?, cstr(to)?);
            let fd = self.fd.as_raw_fd();
            // SAFETY: live descriptors and valid names; no link is followed.
            cvt(unsafe { libc::linkat(fd, f.as_ptr(), fd, t.as_ptr(), 0) })?;
            Ok(())
        }

        pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            let (from, to) = (OsStr::new(from), OsStr::new(to));
            plain(from)?;
            plain(to)?;
            let (f, t) = (cstr(from)?, cstr(to)?);
            let fd = self.fd.as_raw_fd();
            // SAFETY: live descriptors and valid names.
            cvt(unsafe { libc::renameat(fd, f.as_ptr(), fd, t.as_ptr()) })?;
            Ok(())
        }

        /// Make this directory's entries durable: `fsync` on the held
        /// descriptor.
        pub(crate) fn sync(&self) -> io::Result<()> {
            // SAFETY: a live descriptor.
            cvt(unsafe { libc::fsync(self.fd.as_raw_fd()) })?;
            Ok(())
        }

        /// Move `from` in this directory to `to` in `dest`, refusing to
        /// replace an existing `to` (`EEXIST`). One rename between two held
        /// directories: `renameat2(RENAME_NOREPLACE)` on Linux,
        /// `renameatx_np(RENAME_EXCL)` on macOS. A filesystem or platform
        /// without a no-replace rename answers its own error (`EINVAL`,
        /// `ENOSYS`, `ENOTSUP`); there is no fallback to an overwriting rename
        /// or to a link and an unlink.
        pub(crate) fn rename_into(
            &self,
            from: &str,
            dest: &TrustedDir,
            to: &str,
        ) -> io::Result<()> {
            let (from, to) = (OsStr::new(from), OsStr::new(to));
            plain(from)?;
            plain(to)?;
            let (f, t) = (cstr(from)?, cstr(to)?);
            let (from_fd, to_fd) = (self.fd.as_raw_fd(), dest.fd.as_raw_fd());
            #[cfg(test)]
            if let Some(errno) =
                crate::platform::trusted_dir::test_seams::take_rename_into_failure()
            {
                return Err(io::Error::from_raw_os_error(errno));
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            // SAFETY: live descriptors and valid names; no link is followed.
            cvt(unsafe {
                libc::renameat2(
                    from_fd,
                    f.as_ptr(),
                    to_fd,
                    t.as_ptr(),
                    libc::RENAME_NOREPLACE as _,
                )
            })?;
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            // SAFETY: as above.
            cvt(unsafe {
                libc::renameatx_np(from_fd, f.as_ptr(), to_fd, t.as_ptr(), libc::RENAME_EXCL)
            })?;
            #[cfg(not(any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                target_os = "ios"
            )))]
            {
                let _ = (from_fd, to_fd, f, t);
                return Err(io::Error::from_raw_os_error(libc::ENOSYS));
            }
            #[allow(unreachable_code)]
            Ok(())
        }

        pub(crate) fn remove_file(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
            let name = name.as_ref();
            plain(name)?;
            let c = cstr(name)?;
            // SAFETY: a live descriptor and a valid name; a link is
            // removed, never followed.
            cvt(unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) })?;
            Ok(())
        }

        pub(crate) fn remove_dir(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
            let name = name.as_ref();
            plain(name)?;
            let c = cstr(name)?;
            // SAFETY: as above.
            cvt(unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) })?;
            Ok(())
        }

        /// What `name` is, without following a link or opening it.
        pub(crate) fn entry(&self, name: impl AsRef<OsStr>) -> io::Result<EntryInfo> {
            let name = name.as_ref();
            plain(name)?;
            let c = cstr(name)?;
            let mut st = MaybeUninit::<libc::stat>::uninit();
            // SAFETY: a live descriptor, a valid name and storage for one
            // `stat`, initialised by a successful call.
            cvt(unsafe {
                libc::fstatat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    st.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            })?;
            // SAFETY: fstatat succeeded and filled it.
            let st = unsafe { st.assume_init() };
            let kind = match st.st_mode & libc::S_IFMT {
                libc::S_IFREG => EntryKind::File,
                libc::S_IFDIR => EntryKind::Dir,
                _ => EntryKind::Other,
            };
            Ok(EntryInfo {
                kind,
                len: u64::try_from(st.st_size).unwrap_or(0),
            })
        }

        /// Open a regular file for reading: no link followed, no open that
        /// blocks on a FIFO, and refused when it is not a regular file.
        pub(crate) fn open_regular(&self, name: impl AsRef<OsStr>) -> io::Result<File> {
            let name = name.as_ref();
            plain(name)?;
            let c = cstr(name)?;
            // SAFETY: a live descriptor and a valid name; owned below.
            let fd = cvt(unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            })?;
            // SAFETY: just opened, owned by nobody else.
            let file = unsafe { File::from_raw_fd(fd) };
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "it is not a regular file",
                ));
            }
            Ok(file)
        }

        /// A directory stream over a fresh open of this directory, so two
        /// streams never share a read position.
        pub(crate) fn stream(&self) -> io::Result<DirStream> {
            let dot = cstr(OsStr::new("."))?;
            // SAFETY: a live descriptor and a valid name.
            let fd = cvt(unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            })?;
            // SAFETY: `fd` is ours; fdopendir takes it over on success.
            let dir = unsafe { libc::fdopendir(fd) };
            if dir.is_null() {
                let error = io::Error::last_os_error();
                // SAFETY: fdopendir failed, so `fd` is still ours to close.
                unsafe { libc::close(fd) };
                return Err(error);
            }
            Ok(DirStream { dir })
        }
    }

    #[derive(Debug)]
    pub(crate) struct DirStream {
        dir: *mut libc::DIR,
    }

    // SAFETY: the stream is used by one owner at a time; a `DIR` may move
    // between threads.
    unsafe impl Send for DirStream {}

    impl DirStream {
        /// The next name as bytes, `None` at the end of the directory, or
        /// the error `readdir` reported (`EILSEQ` included): an error is
        /// never mistaken for the end.
        pub(crate) fn next_name(&mut self) -> Option<io::Result<OsString>> {
            loop {
                clear_errno();
                // SAFETY: `dir` is a live stream owned by self.
                let entry = unsafe { libc::readdir(self.dir) };
                if entry.is_null() {
                    let error = io::Error::last_os_error();
                    return match error.raw_os_error() {
                        None | Some(0) => None,
                        Some(_) => Some(Err(error)),
                    };
                }
                // SAFETY: readdir returned a valid entry whose name is
                // NUL-terminated and lives until the next readdir.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                return Some(Ok(OsString::from_vec(name.to_vec())));
            }
        }

        pub(crate) fn rewind(&mut self) -> io::Result<()> {
            // SAFETY: `dir` is a live stream owned by self.
            unsafe { libc::rewinddir(self.dir) };
            Ok(())
        }
    }

    impl Drop for DirStream {
        fn drop(&mut self) {
            // SAFETY: `dir` is a live stream owned by self, closed once.
            unsafe { libc::closedir(self.dir) };
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::{OsStr, OsString};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    use super::{EntryInfo, EntryKind, owner_only, plain};
    use super::{refused, valid_component};

    /// An open directory handle and the handles of every component above
    /// it, back to the anchor. Each is opened **without**
    /// `FILE_SHARE_DELETE`, so while it is held no component of the chain
    /// can be renamed or deleted. Write sharing stays allowed. The reason
    /// it was kept is unverified: the belief that linking or renaming
    /// into a directory opens it for writing, and that dropping write
    /// sharing would not stop `FSCTL_SET_REPARSE_POINT` anyway, since a
    /// handle asking only for attribute access is not checked against
    /// share mode. inbox/receipt/017 makes the mount point through a
    /// generic writable handle, so it cannot tell the two apart. A
    /// same-user writer can make a held, empty directory a mount point in
    /// place; [`TrustedDir::guarded`] detects one that is there when it
    /// looks.
    #[derive(Debug)]
    struct Held {
        dir: File,
        parent: Option<Arc<Held>>,
    }

    fn open_dir(path: &Path, follow: bool) -> io::Result<File> {
        let flags = if follow {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
        };
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(flags)
            .open(path)
    }

    fn handle(file: &File) -> HANDLE {
        file.as_raw_handle() as HANDLE
    }

    /// The opened handle itself is a directory and not a reparse point,
    /// and its owner is this user.
    fn verify_dir(file: &File, what: &str) -> io::Result<()> {
        let attributes = file.metadata()?.file_attributes();
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(refused(format!("{what} is a reparse point")));
        }
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(refused(format!("{what} is not a directory")));
        }
        crate::platform::fsperm::verify_file_owner_is_current_user(handle(file))
            .map_err(|why| refused(format!("{what}: {why}")))
    }

    /// A verified directory, held as a handle chain back to the anchor.
    /// Windows has no `openat`, so operations go by path. Every component
    /// of that path is held open without delete sharing, so it cannot be
    /// renamed or deleted, and every operation checks through those
    /// handles, before and after, that none is a reparse point. That
    /// detects a reparse point present when a check looks; it is not
    /// confinement. A postcheck cannot undo a create, link, rename or
    /// remove that already went through a redirected path, and a junction
    /// installed and removed between the two checks is not seen.
    #[derive(Debug)]
    pub(crate) struct TrustedDir {
        path: PathBuf,
        held: Arc<Held>,
    }

    impl TrustedDir {
        /// The trust anchor: a directory owned by this user, opened by
        /// path (its ancestors are trusted as the OS resolves them) and
        /// held.
        pub(crate) fn anchor(path: &Path) -> io::Result<Self> {
            let dir = open_dir(path, true)?;
            if dir.metadata()?.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(refused(format!("{} is not a directory", path.display())));
            }
            crate::platform::fsperm::verify_file_owner_is_current_user(handle(&dir))
                .map_err(|why| refused(format!("{}: {why}", path.display())))?;
            Ok(TrustedDir {
                path: path.to_path_buf(),
                held: Arc::new(Held { dir, parent: None }),
            })
        }

        /// Every held handle of the chain still names a directory that is
        /// not a reparse point, read through the handle itself, so an
        /// in-place `FSCTL_SET_REPARSE_POINT` on any component is seen.
        fn intact(&self) -> io::Result<()> {
            let mut held = Some(&self.held);
            while let Some(h) = held {
                if h.dir.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(refused(format!(
                        "a spool directory at or above {} became a reparse point",
                        self.path.display()
                    )));
                }
                held = h.parent.as_ref();
            }
            Ok(())
        }

        /// Run one path operation between two [`Self::intact`] checks:
        /// refused when a component is a reparse point before it, and an
        /// error after it even when it succeeded, since it may then have
        /// been redirected. The error does not undo what the redirected
        /// operation did. A reparse point both made and removed between
        /// the two checks is not seen.
        fn guarded<T>(&self, op: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
            self.intact()?;
            let result = op(&self.path);
            self.intact()?;
            result
        }

        fn below(&self, path: PathBuf, dir: File) -> Self {
            TrustedDir {
                path,
                held: Arc::new(Held {
                    dir,
                    parent: Some(self.held.clone()),
                }),
            }
        }

        /// One component below: created with a protected owner-only DACL
        /// in the same call when `create`, then opened without following
        /// a reparse point and held; refused unless the handle is a
        /// directory owned by this user whose stored DACL is protected and
        /// owner-only. A pre-existing directory that fails is refused,
        /// never repaired.
        pub(crate) fn child(&self, name: &str, create: bool) -> io::Result<Self> {
            if !valid_component(name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name:?} is not a spool path component"),
                ));
            }
            let path = self.path.join(name);
            let dir = self.guarded(|_| {
                if create {
                    crate::platform::fsperm::create_dir_owner_only(&path)?;
                }
                open_dir(&path, false)
            })?;
            verify_dir(&dir, name)?;
            crate::platform::fsperm::verify_dacl_owner_only(handle(&dir))
                .map_err(|why| refused(format!("{name}: {why}")))?;
            Ok(self.below(path, dir))
        }

        /// A directory below that is about to be removed: not a reparse
        /// point and owned by this user, any DACL.
        pub(crate) fn child_for_removal(&self, name: &OsStr) -> io::Result<Self> {
            plain(name)?;
            let path = self.path.join(name);
            let dir = self.guarded(|_| open_dir(&path, false))?;
            verify_dir(&dir, &format!("{name:?}"))?;
            Ok(self.below(path, dir))
        }

        pub(crate) fn try_clone(&self) -> io::Result<Self> {
            Ok(TrustedDir {
                path: self.path.clone(),
                held: self.held.clone(),
            })
        }

        /// A new file whose owner-only DACL is set on its handle before
        /// anything is written.
        pub(crate) fn create_new_owner_only(&self, name: &str) -> io::Result<File> {
            plain(OsStr::new(name))?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            crate::platform::fsperm::set_create_mode_owner_only(&mut options);
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
            let file = self.guarded(|dir| options.open(dir.join(name)))?;
            if let Err(error) = owner_only(&file) {
                drop(file);
                let _ = self.remove_file(name);
                return Err(error);
            }
            Ok(file)
        }

        pub(crate) fn link(&self, from: &str, to: &str) -> io::Result<()> {
            plain(OsStr::new(from))?;
            plain(OsStr::new(to))?;
            self.guarded(|dir| std::fs::hard_link(dir.join(from), dir.join(to)))
        }

        pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            plain(OsStr::new(from))?;
            plain(OsStr::new(to))?;
            self.guarded(|dir| std::fs::rename(dir.join(from), dir.join(to)))
        }

        /// A documented no-op: Windows has no directory `fsync`; a rename's
        /// durability is the filesystem's.
        pub(crate) fn sync(&self) -> io::Result<()> {
            Ok(())
        }

        /// Move `from` here to `to` in `dest` without replacing an existing
        /// `to`: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, between
        /// both directories' path checks. Path-based, like every operation
        /// here: detection, not confinement.
        pub(crate) fn rename_into(
            &self,
            from: &str,
            dest: &TrustedDir,
            to: &str,
        ) -> io::Result<()> {
            use std::os::windows::ffi::OsStrExt as _;
            plain(OsStr::new(from))?;
            plain(OsStr::new(to))?;
            let wide = |path: PathBuf| -> Vec<u16> {
                path.as_os_str()
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect()
            };
            dest.intact()?;
            let target = wide(dest.path.join(to));
            let moved = self.guarded(|dir| {
                let source = wide(dir.join(from));
                // SAFETY: two NUL-terminated wide paths that outlive the call.
                let ok = unsafe {
                    windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                        source.as_ptr(),
                        target.as_ptr(),
                        0,
                    )
                };
                if ok == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
            dest.intact()?;
            moved
        }

        pub(crate) fn remove_file(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
            plain(name.as_ref())?;
            self.guarded(|dir| std::fs::remove_file(dir.join(name.as_ref())))
        }

        pub(crate) fn remove_dir(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
            plain(name.as_ref())?;
            self.guarded(|dir| std::fs::remove_dir(dir.join(name.as_ref())))
        }

        pub(crate) fn entry(&self, name: impl AsRef<OsStr>) -> io::Result<EntryInfo> {
            plain(name.as_ref())?;
            let meta = self.guarded(|dir| std::fs::symlink_metadata(dir.join(name.as_ref())))?;
            let reparse = meta.file_type().is_symlink()
                || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            let kind = if reparse {
                EntryKind::Other
            } else if meta.is_file() {
                EntryKind::File
            } else if meta.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::Other
            };
            Ok(EntryInfo {
                kind,
                len: meta.len(),
            })
        }

        pub(crate) fn open_regular(&self, name: impl AsRef<OsStr>) -> io::Result<File> {
            let name = name.as_ref();
            if self.entry(name)?.kind != EntryKind::File {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "it is not a regular file",
                ));
            }
            let file = self.guarded(|dir| {
                OpenOptions::new()
                    .read(true)
                    .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(dir.join(name))
            })?;
            let meta = file.metadata()?;
            if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "it is not a regular file",
                ));
            }
            Ok(file)
        }

        pub(crate) fn stream(&self) -> io::Result<DirStream> {
            Ok(DirStream {
                dir: self.try_clone()?,
                iter: Some(self.guarded(|dir| std::fs::read_dir(dir))?),
            })
        }
    }

    #[derive(Debug)]
    pub(crate) struct DirStream {
        dir: TrustedDir,
        iter: Option<std::fs::ReadDir>,
    }

    impl DirStream {
        /// The next name, `None` at the end of the directory, or a
        /// per-entry error: an error is never mistaken for the end, and a
        /// stream a failed rewind lost reports an error, not the end.
        pub(crate) fn next_name(&mut self) -> Option<io::Result<OsString>> {
            let Some(iter) = self.iter.as_mut() else {
                return Some(Err(io::Error::other(
                    "the directory stream was lost by a failed rewind",
                )));
            };
            Some(iter.next()?.map(|entry| entry.file_name()))
        }

        pub(crate) fn rewind(&mut self) -> io::Result<()> {
            self.iter = None;
            self.iter = Some(self.dir.guarded(|dir| std::fs::read_dir(dir))?);
            Ok(())
        }
    }
}

/// Test seams for the operations this module adds.
#[cfg(test)]
pub(crate) mod test_seams {
    use std::cell::Cell;

    std::thread_local! {
        static RENAME_INTO_FAILURE: Cell<Option<i32>> = const { Cell::new(None) };
        static OWNER_ONLY_FAILURE: Cell<bool> = const { Cell::new(false) };
    }

    /// The next `create_new_owner_only` on this thread creates its file and
    /// then fails to set it owner-only.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn fail_next_owner_only() {
        OWNER_ONLY_FAILURE.with(|slot| slot.set(true));
    }

    pub(super) fn take_owner_only_failure() -> bool {
        OWNER_ONLY_FAILURE.with(Cell::take)
    }

    /// The next `rename_into` on this thread fails with `errno`, moving
    /// nothing.
    pub(crate) fn fail_next_rename_into(errno: i32) {
        RENAME_INTO_FAILURE.with(|slot| slot.set(Some(errno)));
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn take_rename_into_failure() -> Option<i32> {
        RENAME_INTO_FAILURE.with(Cell::take)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read as _, Write as _};

    use spec::spec;

    use super::*;

    fn write(dir: &TrustedDir, name: &str, body: &str) {
        let mut file = dir.create_new_owner_only(name).expect("create");
        file.write_all(body.as_bytes()).expect("write");
    }

    fn read(dir: &TrustedDir, name: &str) -> String {
        let mut text = String::new();
        dir.open_regular(name)
            .expect("open")
            .read_to_string(&mut text)
            .expect("read");
        text
    }

    /// Scenario: Two held directories under one anchor. `rename_into` moves a file from one to the other, refuses with `EEXIST` to replace a file already at the destination name (both files unchanged), and `sync` succeeds on a held directory.
    #[spec("inbox/recovery/018")]
    #[test]
    fn recovery_018_rename_into_moves_between_held_directories_and_never_replaces() {
        let root = crate::test_temp::tempdir().unwrap();
        let anchor = TrustedDir::anchor(root.path()).unwrap();
        let from = anchor.child("from", true).unwrap();
        let to = anchor.child("to", true).unwrap();
        write(&from, "r-one.md", "first");
        from.rename_into("r-one.md", &to, "r-one.d100.p.md")
            .unwrap();
        assert_eq!(read(&to, "r-one.d100.p.md"), "first");
        assert_eq!(
            from.entry("r-one.md").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        write(&from, "r-two.md", "second");
        write(&to, "r-two.md", "already here");
        let refused = from.rename_into("r-two.md", &to, "r-two.md").unwrap_err();
        assert_eq!(refused.raw_os_error(), Some(libc::EEXIST));
        assert_eq!(read(&from, "r-two.md"), "second");
        assert_eq!(read(&to, "r-two.md"), "already here");

        assert_eq!(
            from.rename_into("../escape", &to, "x").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        from.sync().unwrap();
        to.sync().unwrap();
    }

    /// Scenario: After both directories are held, their parent is renamed away and an impostor directory tree put in its place. `rename_into` still moves within the pinned originals, and the impostor receives nothing.
    #[spec("inbox/recovery/018")]
    #[test]
    fn recovery_018_rename_into_after_a_parent_swap_stays_in_the_pinned_originals() {
        let root = crate::test_temp::tempdir().unwrap();
        let anchor = TrustedDir::anchor(root.path()).unwrap();
        let parent = anchor.child("store", true).unwrap();
        let from = parent.child("from", true).unwrap();
        let to = parent.child("to", true).unwrap();
        write(&from, "r-pinned.md", "pinned");

        std::fs::rename(root.path().join("store"), root.path().join("moved-away")).unwrap();
        for dir in ["store", "store/from", "store/to"] {
            std::fs::create_dir(root.path().join(dir)).unwrap();
        }

        from.rename_into("r-pinned.md", &to, "r-pinned.d5.r.md")
            .unwrap();
        assert!(root.path().join("moved-away/to/r-pinned.d5.r.md").is_file());
        assert_eq!(
            std::fs::read_dir(root.path().join("store/to"))
                .unwrap()
                .count(),
            0,
            "the impostor got nothing"
        );
    }

    /// Scenario: An injected unsupported-rename error (`ENOSYS`) from `rename_into`. The error is returned as is, and the file is still in its source under its old name, with no second name at the destination.
    #[spec("inbox/recovery/018")]
    #[test]
    fn recovery_018_an_unsupported_rename_moves_nothing() {
        let root = crate::test_temp::tempdir().unwrap();
        let anchor = TrustedDir::anchor(root.path()).unwrap();
        let from = anchor.child("from", true).unwrap();
        let to = anchor.child("to", true).unwrap();
        write(&from, "r-stays.md", "stays");
        test_seams::fail_next_rename_into(libc::ENOSYS);
        let error = from
            .rename_into("r-stays.md", &to, "r-stays.md")
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSYS));
        assert_eq!(read(&from, "r-stays.md"), "stays");
        assert_eq!(
            to.entry("r-stays.md").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
