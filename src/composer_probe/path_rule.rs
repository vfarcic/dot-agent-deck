//! PRD #1666 part 3 (Q35-03; R5 §6.1, §6.2, §6.4, R6 §6.3–§6.5, core §6.1):
//! whether the deck may trust the file an agent exports for it to read, and
//! the opened file it then reads.
//!
//! The rule walks the export's path from `/`, opening each component relative
//! to the one before it without following a link, and checks what it opened
//! rather than the name. [`FsView`] is the seam: the walk is written against
//! it, a scripted view drives the tests, and the Linux view is
//! `platform::fsperm::fs_view`. Nothing in the daemon calls it yet (Q35-13
//! and Q35-14a will).
//!
//! **What it defends against** (R6 §6.5): another uid, members of the user's
//! groups included, creating, renaming or rewriting the export or an entry on
//! its path between the agent's write, the deck's read and the agent's
//! reimport. Same-uid processes and root are inside the boundary: they can
//! already change the agent's configuration.
//!
//! **Every directory, `/` included,** must:
//! - be on an allow-listed filesystem ([`ALLOWED_FILESYSTEMS`]: ext2/3/4, XFS,
//!   Btrfs, tmpfs). Anything else refuses, network filesystems, FUSE,
//!   overlayfs and ZFS among them, since local mode bits aren't the boundary
//!   there; an allow-listed directory reached through another filesystem
//!   refuses too (core §6.1).
//! - be owned by root or the user;
//! - have a POSIX ACL that is absent or one [`acl_grants`] reads as a whole,
//!   valid ACL, and no NFSv4 or rich ACL. A directory with no ACL reads `ENODATA` on a
//!   filesystem with POSIX ACLs; `ENOTSUP` is unexpected on these and
//!   refuses.
//! - and either (a) have no group or other write bit and no ACL write grant,
//!   or (b) be root's, sticky and other-writable, like `/tmp`, where a grant
//!   adds nothing. After (b) the next component
//!   must be the user's and pass (a): (b) doesn't chain, and no link is
//!   followed out of it.
//!
//! No branch reads group membership: a group-writable directory refuses
//! whoever is in the group, since membership can't be enumerated reliably
//! (a primary gid isn't listed in `gr_mem`).
//!
//! **A symlink** on the way is followed only when it is root's or the user's,
//! its directory passed (a), and the walk has followed at most
//! [`MAX_SYMLINK_HOPS`]; its target is walked under the same rules, from `/`
//! if absolute. `..` goes back to the directory the walk came from, as the
//! kernel resolves it.
//!
//! **The file** is opened from its checked directory without following a
//! link and checked on what was opened: a regular file, the user's, one link,
//! on an allow-listed filesystem, with a readable ACL, and not writable by
//! anyone else who can reach it. Either it has no group or other write bit
//! and no ACL write grant, or its directory denies group and others search
//! (no `0o011` bit and no ACL execute grant), so no other uid but root can
//! open it by path now and one link leaves no other path. That second branch is what
//! accepts Claude's 0664 export in its 0700 directory; it doesn't prove no
//! other uid kept a descriptor from a time the directory was searchable,
//! which only the user could have arranged (R6 §6.4). A directory that
//! passed by (b) leaves the file only the first branch.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};

/// The filesystems the rule accepts, by `statfs`'s `f_type` (R6 §6.3).
pub const ALLOWED_FILESYSTEMS: &[(u32, &str)] = &[
    (0xEF53, "ext2/3/4"),
    (0x5846_5342, "XFS"),
    (0x9123_683E, "Btrfs"),
    (0x0102_1994, "tmpfs"),
];

/// How many symlinks one walk follows before it refuses.
pub const MAX_SYMLINK_HOPS: usize = 40;

/// The POSIX access ACL's extended attribute.
pub const POSIX_ACL_ACCESS: &str = "system.posix_acl_access";

/// ACL models whose presence refuses: the rule reads only POSIX ACLs.
pub const FOREIGN_ACLS: &[&str] = &["system.nfs4_acl", "system.richacl"];

const STICKY: u32 = 0o1000;
const GROUP_OTHER_WRITE: u32 = 0o022;
const OTHER_WRITE: u32 = 0o002;
const GROUP_OTHER_SEARCH: u32 = 0o011;

/// What an opened node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    /// A FIFO, socket or device.
    Other,
}

/// `fstat` of an opened node, as far as the rule reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    /// The permission bits, setuid, setgid and sticky included (`0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub nlink: u64,
}

/// One extended attribute read from an opened node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Xattr {
    /// `ENODATA`: the node has no such attribute.
    Absent,
    /// `ENOTSUP`: the filesystem has no such attribute at all.
    Unsupported,
    Present(Vec<u8>),
}

/// What a lookup of one name, without following it, found.
#[derive(Debug)]
pub enum Entry<H> {
    /// Opened as asked: a directory for [`FsView::open_dir`], anything that
    /// opens for [`FsView::open_file`] (its kind is checked afterwards).
    Opened(H),
    /// A symlink: its owner and its target.
    Symlink { uid: u32, target: OsString },
    /// Neither: a file where a directory was wanted.
    Other,
}

/// The filesystem as the rule reads it: `openat` without following, `fstat`,
/// `fstatfs` and `fgetxattr`, all on handles.
pub trait FsView {
    type Handle;

    /// `/`, opened as a directory.
    fn open_root(&mut self) -> io::Result<Self::Handle>;
    /// `name` in `dir`, opened as a directory without following a symlink
    /// (`O_DIRECTORY | O_NOFOLLOW`).
    fn open_dir(&mut self, dir: &Self::Handle, name: &OsStr) -> io::Result<Entry<Self::Handle>>;
    /// `name` in `dir`, opened read-only without following a symlink or
    /// blocking on a FIFO (`O_RDONLY | O_NOFOLLOW | O_NONBLOCK`).
    fn open_file(&mut self, dir: &Self::Handle, name: &OsStr) -> io::Result<Entry<Self::Handle>>;
    fn stat(&mut self, node: &Self::Handle) -> io::Result<Stat>;
    /// `statfs`'s `f_type`.
    fn fs_type(&mut self, node: &Self::Handle) -> io::Result<u32>;
    fn xattr(&mut self, node: &Self::Handle, name: &str) -> io::Result<Xattr>;
}

/// The syscall a refusal for an I/O error came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Open,
    Stat,
    FsType,
    Xattr,
}

/// Why a path is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// The export's path isn't absolute.
    NotAbsolute,
    /// The path names no file (it ends at `/` or `..`).
    NoFileName,
    /// The node is on a filesystem outside [`ALLOWED_FILESYSTEMS`].
    Filesystem { f_type: u32 },
    /// A directory or symlink owned by neither root nor the user.
    ForeignOwner { uid: u32 },
    /// After a shared sticky directory, a component that isn't the user's.
    NotYoursInSticky { uid: u32 },
    /// A directory with a group or other write bit that isn't root's sticky
    /// directory; or, after one, any write bit.
    Writable { mode: u32, uid: u32 },
    /// A POSIX ACL entry grants write to someone besides the owner.
    AclWrite,
    /// The POSIX ACL can't be read on a filesystem that should have it.
    AclUnsupported,
    /// The POSIX ACL attribute isn't a whole, valid ACL ([`acl_grants`]).
    AclMalformed,
    /// An NFSv4 or rich ACL is present.
    ForeignAcl { name: &'static str },
    /// A symlink in a shared sticky directory, as the export itself, or with
    /// an empty target.
    Symlink,
    /// More than [`MAX_SYMLINK_HOPS`] symlinks.
    TooManySymlinks,
    /// A file where a directory belongs.
    NotADirectory,
    /// The export isn't a regular file.
    NotARegularFile,
    /// The export isn't the user's.
    FileNotYours { uid: u32 },
    /// The export has more than one link.
    Links { nlink: u64 },
    /// The export can be written by others who can reach it.
    FileWritable { mode: u32 },
    /// A syscall failed.
    Io { op: Op, errno: Option<i32> },
}

/// A refused path: the component the rule stopped at and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The component as the walk reached it, symlinks resolved.
    pub at: PathBuf,
    pub cause: Cause,
}

/// What the user can do about a refusal (core §6.3; the words are Q35-10's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remedy {
    /// Remove these write bits from the refused directory, which is the
    /// user's: CX-12 option C's `chmod g-w`, Q47 item 12.
    RemoveWrite { bits: u32 },
    /// Point `TMPDIR` or `CODEX_HOME` at a directory that passes.
    Relocate,
}

impl Refusal {
    pub fn remedy(&self, uid: u32) -> Remedy {
        match self.cause {
            Cause::Writable { mode, uid: owner } if owner == uid => Remedy::RemoveWrite {
                bits: mode & GROUP_OTHER_WRITE,
            },
            _ => Remedy::Relocate,
        }
    }
}

/// A file the rule accepted: the handle it opened and checked, to be read
/// through and never reopened by path.
#[derive(Debug)]
pub struct Verified<H> {
    pub file: H,
    pub stat: Stat,
}

/// What a POSIX ACL grants beyond its owner, the owning group and others
/// ([`acl_grants`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AclGrants {
    pub write: bool,
    pub execute: bool,
}

const ACL_XATTR_VERSION: u32 = 2;
const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;
const ACL_WRITE: u16 = 0x02;
const ACL_EXECUTE: u16 = 0x01;
/// Read, write and execute: every permission bit an entry may carry.
const ACL_PERMISSIONS: u16 = 0o7;
/// The id an entry without one carries, and no named entry may.
const ACL_UNDEFINED_ID: u32 = u32::MAX;

/// Where [`acl_grants`] is in an ACL's entries, which come in the kernel's
/// order: the owner, named users, the owning group, named groups, the mask,
/// others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AclAt {
    Start,
    Users,
    Groups,
    Masked,
    Done,
}

/// Read `system.posix_acl_access`'s value: a little-endian version 2 header,
/// then entries of tag, permissions and id. A named user or group entry, or
/// the mask, carrying write or execute grants it. The owner, owning-group
/// and other entries are the mode bits, which the rule reads directly; the
/// owning group's effective rights are capped by the mask, which is read
/// here.
///
/// `None`, which refuses, unless the value is a whole, well-formed ACL: the
/// version-2 header and whole entries after it; one owner entry first, then
/// named users, one owning-group entry, named groups, a mask (required when
/// there is a named entry, and at most one) and one other entry last; no tag
/// outside those six; permissions within read, write and execute; an id on
/// each named entry and on no other; and no named user or group twice. That
/// makes the value's length its entry count. The tag order, the mask rule
/// and the permission check follow Linux's `posix_acl_valid`; the header and
/// framing are `posix_acl_from_xattr`'s. Two checks are stricter than the
/// kernel's: a named user or group twice, and an id on an entry that isn't
/// named, both refuse here. Named ids out of order are accepted, as the
/// kernel accepts them; grants are read from every entry, so order can't
/// hide one.
pub fn acl_grants(value: &[u8]) -> Option<AclGrants> {
    let (header, entries) = value.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*header) != ACL_XATTR_VERSION || !entries.len().is_multiple_of(8) {
        return None;
    }
    let mut grants = AclGrants::default();
    let mut named = std::collections::BTreeSet::new();
    let mut at = AclAt::Start;
    for entry in entries.as_chunks::<8>().0 {
        let tag = u16::from_le_bytes([entry[0], entry[1]]);
        let perm = u16::from_le_bytes([entry[2], entry[3]]);
        let id = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
        if perm & !ACL_PERMISSIONS != 0 {
            return None;
        }
        let is_named = tag == ACL_USER || tag == ACL_GROUP;
        if is_named == (id == ACL_UNDEFINED_ID) {
            return None;
        }
        at = match (at, tag) {
            (AclAt::Start, ACL_USER_OBJ) | (AclAt::Users, ACL_USER) => AclAt::Users,
            (AclAt::Users, ACL_GROUP_OBJ) | (AclAt::Groups, ACL_GROUP) => AclAt::Groups,
            (AclAt::Groups, ACL_MASK) => AclAt::Masked,
            (AclAt::Masked, ACL_OTHER) => AclAt::Done,
            (AclAt::Groups, ACL_OTHER) if named.is_empty() => AclAt::Done,
            _ => return None,
        };
        if is_named && !named.insert((tag, id)) {
            return None;
        }
        if tag != ACL_USER_OBJ && tag != ACL_GROUP_OBJ && tag != ACL_OTHER {
            grants.write |= perm & ACL_WRITE != 0;
            grants.execute |= perm & ACL_EXECUTE != 0;
        }
    }
    (at == AclAt::Done).then_some(grants)
}

/// How a directory passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    /// (a): nobody else can write it.
    Private,
    /// (b): root's sticky, other-writable directory.
    Sticky,
}

struct Level<H> {
    handle: H,
    path: PathBuf,
    trust: Trust,
    /// Group or others may search it, by mode or ACL.
    searchable: bool,
}

enum Step {
    Name(OsString),
    Parent,
}

fn steps(path: &Path) -> impl Iterator<Item = Step> + '_ {
    path.components().filter_map(|component| match component {
        Component::Normal(name) => Some(Step::Name(name.to_owned())),
        Component::ParentDir => Some(Step::Parent),
        Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
    })
}

fn refuse<T>(at: &Path, cause: Cause) -> Result<T, Refusal> {
    Err(Refusal {
        at: at.to_owned(),
        cause,
    })
}

fn io_refusal(at: &Path, op: Op, error: &io::Error) -> Refusal {
    Refusal {
        at: at.to_owned(),
        cause: Cause::Io {
            op,
            errno: error.raw_os_error(),
        },
    }
}

/// Walk `path` from `/` and open the export, or say where and why the rule
/// refuses it. `uid` is the user's (the daemon's and the agent's).
pub fn verify_export<V: FsView>(
    view: &mut V,
    path: &Path,
    uid: u32,
) -> Result<Verified<V::Handle>, Refusal> {
    if !path.is_absolute() {
        return refuse(path, Cause::NotAbsolute);
    }
    let Some(file_name) = path.file_name() else {
        return refuse(path, Cause::NoFileName);
    };
    let root = Path::new("/");
    let handle = view
        .open_root()
        .map_err(|error| io_refusal(root, Op::Open, &error))?;
    let first = check_dir(view, handle, root.to_owned(), None, uid)?;
    let mut stack = vec![first];
    let mut pending: VecDeque<Step> = path.parent().map(steps).into_iter().flatten().collect();
    let mut hops = 0;
    while let Some(step) = pending.pop_front() {
        let name = match step {
            Step::Parent => {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            Step::Name(name) => name,
        };
        let top = stack.last().expect("the stack always holds `/`");
        let at = top.path.join(&name);
        let entry = view
            .open_dir(&top.handle, &name)
            .map_err(|error| io_refusal(&at, Op::Open, &error))?;
        match entry {
            Entry::Opened(handle) => {
                let trust = top.trust;
                let level = check_dir(view, handle, at, Some(trust), uid)?;
                stack.push(level);
            }
            Entry::Symlink { uid: owner, target } => {
                if top.trust == Trust::Sticky {
                    return refuse(&at, Cause::Symlink);
                }
                if owner != 0 && owner != uid {
                    return refuse(&at, Cause::ForeignOwner { uid: owner });
                }
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return refuse(&at, Cause::TooManySymlinks);
                }
                if target.is_empty() {
                    return refuse(&at, Cause::Symlink);
                }
                let target = Path::new(&target);
                if target.has_root() {
                    stack.truncate(1);
                }
                for step in steps(target).collect::<Vec<_>>().into_iter().rev() {
                    pending.push_front(step);
                }
            }
            Entry::Other => return refuse(&at, Cause::NotADirectory),
        }
    }
    let dir = stack.last().expect("the stack always holds `/`");
    let at = dir.path.join(file_name);
    let entry = view
        .open_file(&dir.handle, file_name)
        .map_err(|error| io_refusal(&at, Op::Open, &error))?;
    let file = match entry {
        Entry::Opened(file) => file,
        Entry::Symlink { .. } => return refuse(&at, Cause::Symlink),
        Entry::Other => return refuse(&at, Cause::NotARegularFile),
    };
    let stat = check_file(view, &file, &at, dir, uid)?;
    Ok(Verified { file, stat })
}

/// The checks every opened node gets: its kind, its filesystem and its ACLs.
/// Returns its `fstat` and what its POSIX ACL grants.
fn check_node<V: FsView>(
    view: &mut V,
    node: &V::Handle,
    at: &Path,
    kind: Kind,
) -> Result<(Stat, AclGrants), Refusal> {
    let stat = view
        .stat(node)
        .map_err(|error| io_refusal(at, Op::Stat, &error))?;
    if stat.kind != kind {
        let cause = match kind {
            Kind::Dir => Cause::NotADirectory,
            _ => Cause::NotARegularFile,
        };
        return refuse(at, cause);
    }
    let f_type = view
        .fs_type(node)
        .map_err(|error| io_refusal(at, Op::FsType, &error))?;
    if !ALLOWED_FILESYSTEMS
        .iter()
        .any(|&(allowed, _)| allowed == f_type)
    {
        return refuse(at, Cause::Filesystem { f_type });
    }
    let grants = match view
        .xattr(node, POSIX_ACL_ACCESS)
        .map_err(|error| io_refusal(at, Op::Xattr, &error))?
    {
        Xattr::Absent => AclGrants::default(),
        Xattr::Unsupported => return refuse(at, Cause::AclUnsupported),
        Xattr::Present(value) => match acl_grants(&value) {
            Some(grants) => grants,
            None => return refuse(at, Cause::AclMalformed),
        },
    };
    for &name in FOREIGN_ACLS {
        match view
            .xattr(node, name)
            .map_err(|error| io_refusal(at, Op::Xattr, &error))?
        {
            Xattr::Absent | Xattr::Unsupported => {}
            Xattr::Present(_) => return refuse(at, Cause::ForeignAcl { name }),
        }
    }
    Ok((stat, grants))
}

fn check_dir<V: FsView>(
    view: &mut V,
    handle: V::Handle,
    at: PathBuf,
    parent: Option<Trust>,
    uid: u32,
) -> Result<Level<V::Handle>, Refusal> {
    let (stat, grants) = check_node(view, &handle, &at, Kind::Dir)?;
    if parent == Some(Trust::Sticky) && stat.uid != uid {
        return refuse(&at, Cause::NotYoursInSticky { uid: stat.uid });
    }
    if stat.uid != 0 && stat.uid != uid {
        return refuse(&at, Cause::ForeignOwner { uid: stat.uid });
    }
    let writable = Cause::Writable {
        mode: stat.mode,
        uid: stat.uid,
    };
    let trust = if stat.mode & GROUP_OTHER_WRITE == 0 {
        if grants.write {
            return refuse(&at, Cause::AclWrite);
        }
        Trust::Private
    } else if parent != Some(Trust::Sticky)
        && stat.uid == 0
        && stat.mode & STICKY != 0
        && stat.mode & OTHER_WRITE != 0
    {
        Trust::Sticky
    } else {
        return refuse(&at, writable);
    };
    Ok(Level {
        handle,
        path: at,
        trust,
        searchable: stat.mode & GROUP_OTHER_SEARCH != 0 || grants.execute,
    })
}

fn check_file<V: FsView>(
    view: &mut V,
    file: &V::Handle,
    at: &Path,
    dir: &Level<V::Handle>,
    uid: u32,
) -> Result<Stat, Refusal> {
    let (stat, grants) = check_node(view, file, at, Kind::File)?;
    if stat.uid != uid {
        return refuse(at, Cause::FileNotYours { uid: stat.uid });
    }
    if stat.nlink != 1 {
        return refuse(at, Cause::Links { nlink: stat.nlink });
    }
    let writable_by_others = stat.mode & GROUP_OTHER_WRITE != 0 || grants.write;
    if writable_by_others && (dir.trust == Trust::Sticky || dir.searchable) {
        return refuse(at, Cause::FileWritable { mode: stat.mode });
    }
    Ok(stat)
}
