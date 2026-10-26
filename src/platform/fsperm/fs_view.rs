//! PRD #1666 part 3 (Q35-03): the Linux [`FsView`] the export path rule
//! (`composer_probe::path_rule`) walks: `openat` without following, `fstat`,
//! `fstatfs` and `fgetxattr`, every one on a descriptor the walk holds.
//! Nothing in the daemon uses it yet.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;

use crate::composer_probe::path_rule::{Entry, FsView, Kind, Stat, Xattr};

/// The buffer one `fgetxattr` reads into: Linux's own cap on a value
/// (`XATTR_SIZE_MAX`), so one read gets any value, and `ERANGE` is an error,
/// which the rule refuses.
const MAX_XATTR_BYTES: usize = 64 * 1024;

/// The real filesystem, through descriptors.
#[derive(Debug, Default)]
pub struct LinuxFsView;

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn open_at(dir: Option<&OwnedFd>, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let name = c_name(name)?;
    let dirfd = dir.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
    // SAFETY: `name` is a NUL-terminated string that outlives the call, and
    // `dirfd` is a descriptor `dir` keeps open (or `AT_FDCWD`).
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openat` just returned this descriptor, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn stat_of(fd: &OwnedFd) -> io::Result<Stat> {
    let file = File::from(fd.try_clone()?);
    let metadata = file.metadata()?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        Kind::Dir
    } else if file_type.is_file() {
        Kind::File
    } else if file_type.is_symlink() {
        Kind::Symlink
    } else {
        Kind::Other
    };
    Ok(Stat {
        kind,
        mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        nlink: metadata.nlink(),
    })
}

/// `name` in `dir` that a no-follow open refused with `ELOOP` or `ENOTDIR`:
/// a symlink (its owner and target, read from an `O_PATH` descriptor of the
/// link itself) or something else.
fn classify(dir: &OwnedFd, name: &OsStr) -> io::Result<Entry<OwnedFd>> {
    let link = open_at(Some(dir), name, libc::O_PATH | libc::O_NOFOLLOW)?;
    let stat = stat_of(&link)?;
    if stat.kind != Kind::Symlink {
        return Ok(Entry::Other);
    }
    let mut target = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: an empty path with an `O_PATH` descriptor of a symlink reads
    // that link (Linux 2.6.39 onward); `target` is writable for its length.
    let len = unsafe {
        libc::readlinkat(
            link.as_raw_fd(),
            c"".as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        )
    };
    let len = usize::try_from(len).map_err(|_| io::Error::last_os_error())?;
    if len >= target.len() {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    target.truncate(len);
    Ok(Entry::Symlink {
        uid: stat.uid,
        target: OsString::from_vec(target),
    })
}

fn is_link_or_not_a_directory(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
}

impl FsView for LinuxFsView {
    type Handle = OwnedFd;

    fn open_root(&mut self) -> io::Result<OwnedFd> {
        open_at(None, OsStr::new("/"), libc::O_RDONLY | libc::O_DIRECTORY)
    }

    fn open_dir(&mut self, dir: &OwnedFd, name: &OsStr) -> io::Result<Entry<OwnedFd>> {
        match open_at(
            Some(dir),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        ) {
            Ok(fd) => Ok(Entry::Opened(fd)),
            Err(error) if is_link_or_not_a_directory(&error) => classify(dir, name),
            Err(error) => Err(error),
        }
    }

    fn open_file(&mut self, dir: &OwnedFd, name: &OsStr) -> io::Result<Entry<OwnedFd>> {
        match open_at(
            Some(dir),
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        ) {
            Ok(fd) => Ok(Entry::Opened(fd)),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => classify(dir, name),
            Err(error) => Err(error),
        }
    }

    fn stat(&mut self, node: &OwnedFd) -> io::Result<Stat> {
        stat_of(node)
    }

    fn fs_type(&mut self, node: &OwnedFd) -> io::Result<u32> {
        // SAFETY: `statfs` is plain data, so all-zero is a valid value.
        let mut info: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is writable, and `node` keeps the descriptor open.
        if unsafe { libc::fstatfs(node.as_raw_fd(), &mut info) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Every filesystem magic is 32 bits; `f_type`'s width varies by
        // architecture, and on some it is signed.
        #[allow(clippy::unnecessary_cast)]
        let f_type = info.f_type as u32;
        Ok(f_type)
    }

    fn xattr(&mut self, node: &OwnedFd, name: &str) -> io::Result<Xattr> {
        let name = c_name(OsStr::new(name))?;
        let mut value = vec![0u8; MAX_XATTR_BYTES];
        // SAFETY: `name` is NUL-terminated, `value` is writable for its
        // length, and `node` keeps the descriptor open.
        let read = unsafe {
            libc::fgetxattr(
                node.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        match usize::try_from(read) {
            Ok(read) => {
                value.truncate(read);
                Ok(Xattr::Present(value))
            }
            Err(_) => absent_or_error(io::Error::last_os_error()),
        }
    }
}

fn absent_or_error(error: io::Error) -> io::Result<Xattr> {
    match error.raw_os_error() {
        Some(libc::ENODATA) => Ok(Xattr::Absent),
        Some(libc::EOPNOTSUPP) => Ok(Xattr::Unsupported),
        _ => Err(error),
    }
}
