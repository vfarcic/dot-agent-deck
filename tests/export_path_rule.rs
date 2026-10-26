//! PRD #1666 part 3 (Q35-03): the export path rule
//! (`src/composer_probe/path_rule.rs`). Most cases walk a scripted
//! filesystem, which can hold what a test can't create (another account's
//! directory, a network filesystem, an NFSv4 ACL); the rest walk the real one
//! under `/tmp` through the Linux view.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use dot_agent_deck::composer_probe::path_rule::{
    AclGrants, Cause, Entry, FsView, Kind, MAX_SYMLINK_HOPS, Op, Refusal, Remedy, Stat, Xattr,
    acl_grants, verify_export,
};
use spec::spec;

const ROOT: u32 = 0;
const ME: u32 = 1000;
const OTHER: u32 = 1001;

const EXT4: u32 = 0xEF53;
const XFS: u32 = 0x5846_5342;
const BTRFS: u32 = 0x9123_683E;
const TMPFS: u32 = 0x0102_1994;

const ACL: &str = "system.posix_acl_access";

#[derive(Debug, Clone)]
enum Shape {
    Dir(BTreeMap<OsString, usize>),
    File,
    Fifo,
    Link(OsString),
}

#[derive(Debug, Clone)]
struct Node {
    shape: Shape,
    mode: u32,
    uid: u32,
    nlink: u64,
    fs: u32,
    /// An attribute here is present; `None` means the filesystem doesn't
    /// support it. Missing means absent.
    xattrs: BTreeMap<&'static str, Option<Vec<u8>>>,
    fail: Option<(Op, i32)>,
    /// What `fstat` says it is, when that isn't what opened: an entry
    /// replaced between the open and the stat.
    stat_kind: Option<Kind>,
}

/// A scripted filesystem. Handles are node indices; `/` is node 0, root's,
/// 0755, on ext4.
#[derive(Debug, Clone)]
struct Tree {
    nodes: Vec<Node>,
    root_fails: Option<i32>,
}

impl Tree {
    fn new() -> Self {
        Tree {
            nodes: vec![Node {
                shape: Shape::Dir(BTreeMap::new()),
                mode: 0o755,
                uid: ROOT,
                nlink: 1,
                fs: EXT4,
                xattrs: BTreeMap::new(),
                fail: None,
                stat_kind: None,
            }],
            root_fails: None,
        }
    }

    /// `/home` (root's, 0755), `/home/me` (0750), `/tmp` (root's, 01777,
    /// tmpfs) and `/var/tmp` (root's, 01777).
    fn standard() -> Self {
        let mut tree = Tree::new();
        tree.dir("/home", ROOT, 0o755);
        tree.dir("/home/me", ME, 0o750);
        tree.dir("/tmp", ROOT, 0o1777);
        tree.set_fs("/tmp", TMPFS);
        tree.dir("/var", ROOT, 0o755);
        tree.dir("/var/tmp", ROOT, 0o1777);
        tree
    }

    fn find(&self, path: &str) -> usize {
        let mut node = 0;
        for name in path.split('/').filter(|name| !name.is_empty()) {
            let Shape::Dir(children) = &self.nodes[node].shape else {
                panic!("{path}: not a directory on the way");
            };
            node = children[OsStr::new(name)];
        }
        node
    }

    fn add(&mut self, path: &str, uid: u32, mode: u32, shape: Shape) -> usize {
        let path = Path::new(path);
        let parent = self.find(path.parent().unwrap().to_str().unwrap());
        let node = Node {
            shape,
            mode,
            uid,
            nlink: 1,
            fs: self.nodes[parent].fs,
            xattrs: BTreeMap::new(),
            fail: None,
            stat_kind: None,
        };
        self.nodes.push(node);
        let id = self.nodes.len() - 1;
        let Shape::Dir(children) = &mut self.nodes[parent].shape else {
            panic!("{}: parent isn't a directory", path.display());
        };
        children.insert(path.file_name().unwrap().to_owned(), id);
        id
    }

    fn dir(&mut self, path: &str, uid: u32, mode: u32) -> usize {
        self.add(path, uid, mode, Shape::Dir(BTreeMap::new()))
    }

    fn file(&mut self, path: &str, uid: u32, mode: u32) -> usize {
        self.add(path, uid, mode, Shape::File)
    }

    fn link(&mut self, path: &str, uid: u32, target: &str) -> usize {
        self.add(path, uid, 0o777, Shape::Link(target.into()))
    }

    fn set_fs(&mut self, path: &str, fs: u32) {
        let node = self.find(path);
        self.nodes[node].fs = fs;
    }

    fn set_xattr(&mut self, path: &str, name: &'static str, value: Option<Vec<u8>>) {
        let node = self.find(path);
        self.nodes[node].xattrs.insert(name, value);
    }

    fn fail(&mut self, path: &str, op: Op, errno: i32) {
        let node = self.find(path);
        self.nodes[node].fail = Some((op, errno));
    }

    fn failing(&self, node: usize, op: Op) -> io::Result<()> {
        match self.nodes[node].fail {
            Some((failing, errno)) if failing == op => Err(io::Error::from_raw_os_error(errno)),
            _ => Ok(()),
        }
    }

    fn lookup(&self, dir: usize, name: &OsStr) -> io::Result<usize> {
        let Shape::Dir(children) = &self.nodes[dir].shape else {
            return Err(io::Error::from_raw_os_error(ENOTDIR));
        };
        let node = *children
            .get(name)
            .ok_or_else(|| io::Error::from_raw_os_error(ENOENT))?;
        self.failing(node, Op::Open)?;
        Ok(node)
    }
}

const ENOENT: i32 = 2;
const ENOTDIR: i32 = 20;
const EIO: i32 = 5;

impl FsView for Tree {
    type Handle = usize;

    fn open_root(&mut self) -> io::Result<usize> {
        match self.root_fails {
            Some(errno) => Err(io::Error::from_raw_os_error(errno)),
            None => Ok(0),
        }
    }

    fn open_dir(&mut self, dir: &usize, name: &OsStr) -> io::Result<Entry<usize>> {
        let node = self.lookup(*dir, name)?;
        Ok(match &self.nodes[node].shape {
            Shape::Dir(_) => Entry::Opened(node),
            Shape::Link(target) => Entry::Symlink {
                uid: self.nodes[node].uid,
                target: target.clone(),
            },
            Shape::File | Shape::Fifo => Entry::Other,
        })
    }

    fn open_file(&mut self, dir: &usize, name: &OsStr) -> io::Result<Entry<usize>> {
        let node = self.lookup(*dir, name)?;
        Ok(match &self.nodes[node].shape {
            Shape::Link(target) => Entry::Symlink {
                uid: self.nodes[node].uid,
                target: target.clone(),
            },
            Shape::Dir(_) | Shape::File | Shape::Fifo => Entry::Opened(node),
        })
    }

    fn stat(&mut self, node: &usize) -> io::Result<Stat> {
        self.failing(*node, Op::Stat)?;
        let it = &self.nodes[*node];
        let kind = match it.shape {
            Shape::Dir(_) => Kind::Dir,
            Shape::File => Kind::File,
            Shape::Fifo => Kind::Other,
            Shape::Link(_) => Kind::Symlink,
        };
        Ok(Stat {
            kind: it.stat_kind.unwrap_or(kind),
            mode: it.mode,
            uid: it.uid,
            nlink: it.nlink,
        })
    }

    fn fs_type(&mut self, node: &usize) -> io::Result<u32> {
        self.failing(*node, Op::FsType)?;
        Ok(self.nodes[*node].fs)
    }

    fn xattr(&mut self, node: &usize, name: &str) -> io::Result<Xattr> {
        self.failing(*node, Op::Xattr)?;
        Ok(match self.nodes[*node].xattrs.get(name) {
            None => Xattr::Absent,
            Some(None) => Xattr::Unsupported,
            Some(Some(value)) => Xattr::Present(value.clone()),
        })
    }
}

/// `system.posix_acl_access`'s value for these `(tag, perm, id)` entries.
fn acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut value = 2u32.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        value.extend_from_slice(&tag.to_le_bytes());
        value.extend_from_slice(&perm.to_le_bytes());
        value.extend_from_slice(&id.to_le_bytes());
    }
    value
}

const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const GROUP: u16 = 0x08;
const MASK: u16 = 0x10;
const OTHER_ENTRY: u16 = 0x20;
const UNDEFINED_ID: u32 = u32::MAX;

/// An ACL on a 0755 node, with one named entry (`USER` or `GROUP`, for
/// [`OTHER`]) and a mask, in the kernel's order.
fn acl_with(tag: u16, perm: u16, mask: u16) -> Vec<u8> {
    let named = (tag, perm, OTHER);
    let mut entries = vec![(USER_OBJ, 7, UNDEFINED_ID)];
    if tag == USER {
        entries.push(named);
    }
    entries.push((GROUP_OBJ, 5, UNDEFINED_ID));
    if tag == GROUP {
        entries.push(named);
    }
    entries.extend([(MASK, mask, UNDEFINED_ID), (OTHER_ENTRY, 5, UNDEFINED_ID)]);
    acl(&entries)
}

/// The three entries every ACL has, carrying these permissions.
fn base_acl(user: u16, group: u16, other: u16) -> [(u16, u16, u32); 3] {
    [
        (USER_OBJ, user, UNDEFINED_ID),
        (GROUP_OBJ, group, UNDEFINED_ID),
        (OTHER_ENTRY, other, UNDEFINED_ID),
    ]
}

fn check(tree: &Tree, path: &str, uid: u32) -> Result<usize, Refusal> {
    let mut view = tree.clone();
    verify_export(&mut view, Path::new(path), uid).map(|verified| verified.file)
}

fn refused(tree: &Tree, path: &str, uid: u32) -> Refusal {
    match check(tree, path, uid) {
        Ok(node) => panic!("{path}: accepted (node {node}), expected a refusal"),
        Err(refusal) => refusal,
    }
}

fn refusal(at: &str, cause: Cause) -> Refusal {
    Refusal {
        at: PathBuf::from(at),
        cause,
    }
}

/// Scenario: Walk the exports the agents write on a scripted filesystem: Claude's 0664 file in its 0700 directory under the shared `/tmp`, a 0600 file under a 0755 `TMPDIR` in the home directory, Codex's 0600 file in a 0755 `editor` directory, a file reached through a safe symlink into `/var/tmp`, through a relative link with `..`, and through a root-owned link in `/`. Each one passes, and the handle returned is the file's own.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_the_agents_exports_pass() {
    let mut tree = Tree::standard();
    tree.dir("/tmp/claude-1000", ME, 0o700);
    let claude = tree.file("/tmp/claude-1000/prompt.md", ME, 0o664);
    assert_eq!(check(&tree, "/tmp/claude-1000/prompt.md", ME), Ok(claude));

    tree.dir("/home/me/tmp", ME, 0o755);
    let home_tmp = tree.file("/home/me/tmp/prompt.md", ME, 0o600);
    assert_eq!(check(&tree, "/home/me/tmp/prompt.md", ME), Ok(home_tmp));

    tree.dir("/home/me/.codex", ME, 0o700);
    tree.dir("/home/me/.codex/editor", ME, 0o755);
    let codex = tree.file("/home/me/.codex/editor/prompt.md", ME, 0o600);
    assert_eq!(
        check(&tree, "/home/me/.codex/editor/prompt.md", ME),
        Ok(codex)
    );

    // A safe symlink target: the user's link in their 0750 home, into the
    // user's 0700 child of the root-sticky `/var/tmp`.
    tree.dir("/var/tmp/u-tmp", ME, 0o700);
    let linked = tree.file("/var/tmp/u-tmp/prompt.md", ME, 0o600);
    tree.link("/home/me/ln", ME, "/var/tmp/u-tmp");
    assert_eq!(check(&tree, "/home/me/ln/prompt.md", ME), Ok(linked));

    // A relative target that climbs with `..`, and `..` in the path itself.
    tree.link("/home/me/up", ME, "../me/tmp");
    assert_eq!(check(&tree, "/home/me/up/prompt.md", ME), Ok(home_tmp));
    assert_eq!(
        check(&tree, "/../home/me/tmp/../tmp/prompt.md", ME),
        Ok(home_tmp)
    );

    // A root-owned link in `/` (macOS's `/var` → `/private/var` shape).
    tree.dir("/private", ROOT, 0o755);
    tree.dir("/private/u", ME, 0o700);
    let private = tree.file("/private/u/prompt.md", ME, 0o600);
    tree.link("/p", ROOT, "private");
    assert_eq!(check(&tree, "/p/u/prompt.md", ME), Ok(private));

    // Every allow-listed filesystem.
    for fs in [EXT4, XFS, BTRFS, TMPFS] {
        let mut tree = tree.clone();
        tree.set_fs("/home/me/tmp", fs);
        tree.set_fs("/home/me/tmp/prompt.md", fs);
        assert_eq!(
            check(&tree, "/home/me/tmp/prompt.md", ME),
            Ok(home_tmp),
            "{fs:#x}"
        );
    }

    // Root, whose own directories are the user's.
    let mut tree = Tree::new();
    tree.dir("/root", ROOT, 0o700);
    let root_file = tree.file("/root/prompt.md", ROOT, 0o600);
    assert_eq!(check(&tree, "/root/prompt.md", ROOT), Ok(root_file));
}

/// Scenario: A group-writable directory on the export's path (0775, 0770, 0730), whatever group owns it: the user's own primary group, which another account can share without being listed as a member, or a group whose name looks private. Each is refused at that directory, and the remedy offered is removing the group write bit; the rule's input has no group at all.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_a_group_writable_directory_refuses_whoever_is_in_the_group() {
    for mode in [0o775, 0o770, 0o730, 0o2775] {
        let mut tree = Tree::standard();
        tree.dir("/home/me/shared", ME, mode);
        tree.file("/home/me/shared/prompt.md", ME, 0o600);
        let refusal_seen = refused(&tree, "/home/me/shared/prompt.md", ME);
        assert_eq!(
            refusal_seen,
            refusal("/home/me/shared", Cause::Writable { mode, uid: ME }),
            "{mode:o}"
        );
        assert_eq!(refusal_seen.remedy(ME), Remedy::RemoveWrite { bits: 0o020 });
    }

    // CX-12 option C: a umask-002 `CODEX_HOME` holds at its first
    // group-writable directory, naming it and the bit to remove.
    let mut tree = Tree::standard();
    tree.dir("/home/me/.codex", ME, 0o700);
    tree.dir("/home/me/.codex/editor", ME, 0o775);
    tree.file("/home/me/.codex/editor/prompt.md", ME, 0o600);
    let held = refused(&tree, "/home/me/.codex/editor/prompt.md", ME);
    assert_eq!(
        held,
        refusal(
            "/home/me/.codex/editor",
            Cause::Writable {
                mode: 0o775,
                uid: ME
            }
        )
    );
    assert_eq!(held.remedy(ME), Remedy::RemoveWrite { bits: 0o020 });
    let mut both = tree.clone();
    let codex_home = both.find("/home/me/.codex");
    both.nodes[codex_home].mode = 0o775;
    assert_eq!(
        refused(&both, "/home/me/.codex/editor/prompt.md", ME).at,
        PathBuf::from("/home/me/.codex")
    );
    let mut fixed = tree.clone();
    let editor = fixed.find("/home/me/.codex/editor");
    fixed.nodes[editor].mode = 0o755;
    assert!(check(&fixed, "/home/me/.codex/editor/prompt.md", ME).is_ok());
}

/// Scenario: Shared directories on the way: a non-sticky 0777 directory of root's, a sticky 0777 directory that isn't root's, a root-sticky directory that isn't other-writable, a second root-sticky directory inside `/tmp`, another account's directory inside `/tmp`, the user's 0770 directory inside `/tmp`, another account's 0755 directory, and an export straight in `/tmp`. Each refuses where the rule says, an 0600 export straight in `/tmp` passes and a 0664 one doesn't, and only a directory the user owns is offered the chmod remedy.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_shared_and_sticky_directories() {
    let cases: [(&str, u32, u32, &str, Cause); 4] = [
        (
            "/srv",
            ROOT,
            0o777,
            "/srv",
            Cause::Writable {
                mode: 0o777,
                uid: ROOT,
            },
        ),
        (
            "/srv",
            ME,
            0o1777,
            "/srv",
            Cause::Writable {
                mode: 0o1777,
                uid: ME,
            },
        ),
        (
            "/srv",
            ROOT,
            0o1775,
            "/srv",
            Cause::Writable {
                mode: 0o1775,
                uid: ROOT,
            },
        ),
        (
            "/srv",
            OTHER,
            0o755,
            "/srv",
            Cause::ForeignOwner { uid: OTHER },
        ),
    ];
    for (dir, uid, mode, at, cause) in cases {
        let mut tree = Tree::standard();
        tree.dir(dir, uid, mode);
        tree.dir(&format!("{dir}/u"), ME, 0o700);
        tree.file(&format!("{dir}/u/prompt.md"), ME, 0o600);
        assert_eq!(
            refused(&tree, &format!("{dir}/u/prompt.md"), ME),
            refusal(at, cause),
            "{dir} {uid} {mode:o}"
        );
    }

    let mut tree = Tree::standard();
    tree.dir("/srv", ROOT, 0o777);
    tree.file("/srv/prompt.md", ME, 0o600);
    assert_eq!(
        refused(&tree, "/srv/prompt.md", ME).remedy(ME),
        Remedy::Relocate
    );

    // Inside the root-sticky `/tmp`, the next directory must be the user's
    // and private: (b) doesn't chain.
    let tmp_cases: [(u32, u32, Cause); 4] = [
        (ROOT, 0o1777, Cause::NotYoursInSticky { uid: ROOT }),
        (OTHER, 0o700, Cause::NotYoursInSticky { uid: OTHER }),
        (
            ME,
            0o770,
            Cause::Writable {
                mode: 0o770,
                uid: ME,
            },
        ),
        (
            ME,
            0o1777,
            Cause::Writable {
                mode: 0o1777,
                uid: ME,
            },
        ),
    ];
    for (uid, mode, cause) in tmp_cases {
        let mut tree = Tree::standard();
        tree.dir("/tmp/d", uid, mode);
        tree.dir("/tmp/d/u", ME, 0o700);
        tree.file("/tmp/d/u/prompt.md", ME, 0o600);
        tree.file("/tmp/d/prompt.md", ME, 0o600);
        assert_eq!(
            refused(&tree, "/tmp/d/u/prompt.md", ME),
            refusal("/tmp/d", cause.clone()),
            "{uid} {mode:o}"
        );
        assert_eq!(
            refused(&tree, "/tmp/d/prompt.md", ME),
            refusal("/tmp/d", cause),
            "{uid} {mode:o}"
        );
    }

    // An export straight in `/tmp`: the sticky bit keeps others from
    // replacing it, so only its own write bits decide.
    let mut tree = Tree::standard();
    let direct = tree.file("/tmp/prompt.md", ME, 0o600);
    assert_eq!(check(&tree, "/tmp/prompt.md", ME), Ok(direct));
    tree.nodes[direct].mode = 0o664;
    assert_eq!(
        refused(&tree, "/tmp/prompt.md", ME),
        refusal("/tmp/prompt.md", Cause::FileWritable { mode: 0o664 })
    );
    tree.nodes[direct].mode = 0o604;
    assert_eq!(check(&tree, "/tmp/prompt.md", ME), Ok(direct));

    // A root-sticky directory nobody else can search still leaves the
    // export only its own write bits.
    let mut tree = Tree::standard();
    tree.dir("/srv", ROOT, 0o1762);
    let unsearchable = tree.file("/srv/prompt.md", ME, 0o664);
    assert_eq!(
        refused(&tree, "/srv/prompt.md", ME),
        refusal("/srv/prompt.md", Cause::FileWritable { mode: 0o664 })
    );
    tree.nodes[unsearchable].mode = 0o600;
    assert_eq!(check(&tree, "/srv/prompt.md", ME), Ok(unsearchable));

    // Running as root, a second root-sticky directory is root's own, and
    // still doesn't chain.
    let mut tree = Tree::standard();
    tree.dir("/tmp/d", ROOT, 0o1777);
    tree.dir("/tmp/d/u", ROOT, 0o700);
    tree.file("/tmp/d/u/prompt.md", ROOT, 0o600);
    assert_eq!(
        refused(&tree, "/tmp/d/u/prompt.md", ROOT),
        refusal(
            "/tmp/d",
            Cause::Writable {
                mode: 0o1777,
                uid: ROOT
            }
        )
    );
}

/// Scenario: POSIX ACLs on the way: a named user or group granted write on an otherwise 0755 directory, a mask with write, and the same on `/`; an ACL the filesystem says it doesn't support; ACL values with a wrong version, a cut-short entry, an unknown tag or no entries; an NFSv4 or rich ACL; an error reading one. Each refuses. A named entry with no write passes, and on the export an ACL write grant refuses only where others can search its directory, which an ACL execute grant on that directory allows.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_acls_refuse_write_grants_and_what_cant_be_read() {
    let base = || {
        let mut tree = Tree::standard();
        tree.dir("/home/me/d", ME, 0o755);
        tree.file("/home/me/d/prompt.md", ME, 0o600);
        tree
    };
    let path = "/home/me/d/prompt.md";

    for value in [
        acl_with(USER, 7, 5),
        acl_with(USER, 2, 5),
        acl_with(GROUP, 6, 5),
        acl_with(USER, 5, 7),
    ] {
        let mut tree = base();
        tree.set_xattr("/home/me/d", ACL, Some(value.clone()));
        assert_eq!(
            refused(&tree, path, ME),
            refusal("/home/me/d", Cause::AclWrite),
            "{value:?}"
        );
        let mut tree = base();
        tree.set_xattr("/", ACL, Some(value));
        assert_eq!(refused(&tree, path, ME), refusal("/", Cause::AclWrite));
    }

    let mut tree = base();
    tree.set_xattr("/home/me/d", ACL, Some(acl_with(USER, 5, 5)));
    assert!(check(&tree, path, ME).is_ok());

    // Root's sticky `/tmp` is writable by everyone already: a grant adds
    // nothing, and what follows it is checked as usual.
    let mut tree = base();
    tree.set_xattr("/tmp", ACL, Some(acl_with(USER, 7, 7)));
    tree.dir("/tmp/u", ME, 0o700);
    let in_tmp = tree.file("/tmp/u/prompt.md", ME, 0o600);
    assert_eq!(check(&tree, "/tmp/u/prompt.md", ME), Ok(in_tmp));

    let mut tree = base();
    tree.set_xattr("/home/me/d", ACL, None);
    assert_eq!(
        refused(&tree, path, ME),
        refusal("/home/me/d", Cause::AclUnsupported)
    );
    let mut tree = base();
    tree.set_xattr("/tmp", ACL, None);
    tree.dir("/tmp/u", ME, 0o700);
    tree.file("/tmp/u/prompt.md", ME, 0o600);
    assert_eq!(
        refused(&tree, "/tmp/u/prompt.md", ME),
        refusal("/tmp", Cause::AclUnsupported)
    );

    let mut wrong_version = acl_with(USER, 5, 5);
    wrong_version[0] = 1;
    let mut cut_short = acl_with(USER, 5, 5);
    cut_short.pop();
    let unknown_tag = acl(&[(USER_OBJ, 7, UNDEFINED_ID), (0x40, 0, 0)]);
    for value in [wrong_version, cut_short, unknown_tag, acl(&[]), vec![2, 0]] {
        let mut tree = base();
        tree.set_xattr("/home/me/d", ACL, Some(value.clone()));
        assert_eq!(
            refused(&tree, path, ME),
            refusal("/home/me/d", Cause::AclMalformed),
            "{value:?}"
        );
    }

    for name in ["system.nfs4_acl", "system.richacl"] {
        let mut tree = base();
        tree.set_xattr("/home/me/d", name, Some(vec![0; 8]));
        assert_eq!(
            refused(&tree, path, ME),
            refusal("/home/me/d", Cause::ForeignAcl { name })
        );
        let mut tree = base();
        tree.set_xattr("/home/me/d", name, None);
        assert!(check(&tree, path, ME).is_ok(), "{name} unsupported");
        let mut tree = base();
        tree.set_xattr(path, name, Some(vec![0; 8]));
        assert_eq!(
            refused(&tree, path, ME),
            refusal(path, Cause::ForeignAcl { name })
        );
    }

    let mut tree = base();
    tree.fail("/home/me/d", Op::Xattr, EIO);
    assert_eq!(
        refused(&tree, path, ME),
        refusal(
            "/home/me/d",
            Cause::Io {
                op: Op::Xattr,
                errno: Some(EIO)
            }
        )
    );

    // On the export: a write grant matters only where others can reach it.
    let mut tree = base();
    tree.set_xattr(path, ACL, Some(acl_with(USER, 6, 6)));
    assert_eq!(
        refused(&tree, path, ME),
        refusal(path, Cause::FileWritable { mode: 0o600 })
    );
    let mut tree = base();
    let d = tree.find("/home/me/d");
    tree.nodes[d].mode = 0o700;
    tree.set_xattr(path, ACL, Some(acl_with(USER, 6, 6)));
    assert!(check(&tree, path, ME).is_ok());
    tree.set_xattr("/home/me/d", ACL, Some(acl_with(USER, 1, 1)));
    assert_eq!(
        refused(&tree, path, ME),
        refusal(path, Cause::FileWritable { mode: 0o600 })
    );
    let mut tree = base();
    tree.set_xattr(path, ACL, None);
    assert_eq!(
        refused(&tree, path, ME),
        refusal(path, Cause::AclUnsupported)
    );
}

/// Scenario: Read raw `system.posix_acl_access` values that are whole, valid ACLs: only named user and group entries and the mask grant anything, write and execute are told apart, the owner, owning-group and other entries grant nothing (they are the mode bits), a mask with no named entry is read, and several named users and groups, out of id order and one id as both a user and a group, are read together.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_acl_values_read_as_grants() {
    let none = AclGrants::default();
    let write = AclGrants {
        write: true,
        execute: false,
    };
    let execute = AclGrants {
        write: false,
        execute: true,
    };
    assert_eq!(acl_grants(&acl(&base_acl(7, 7, 7))), Some(none));
    for (perm, grants) in [(2, write), (1, execute), (4, none)] {
        let [user, group, other] = base_acl(7, 7, 7);
        assert_eq!(
            acl_grants(&acl(&[user, group, (MASK, perm, UNDEFINED_ID), other])),
            Some(grants),
            "a mask of {perm}"
        );
        for tag in [USER, GROUP] {
            assert_eq!(
                acl_grants(&acl_with(tag, perm, 4)),
                Some(grants),
                "{tag:#x} with {perm}"
            );
        }
    }
    let many = acl(&[
        (USER_OBJ, 6, UNDEFINED_ID),
        (USER, 4, OTHER),
        (USER, 4, ROOT),
        (USER, 1, ME),
        (GROUP_OBJ, 4, UNDEFINED_ID),
        (GROUP, 4, OTHER),
        (GROUP, 2, ME),
        (MASK, 4, UNDEFINED_ID),
        (OTHER_ENTRY, 4, UNDEFINED_ID),
    ]);
    assert_eq!(
        acl_grants(&many),
        Some(AclGrants {
            write: true,
            execute: true
        })
    );
}

/// A valid ACL with every kind of entry: owner, a named user, owning group,
/// a named group, the mask and others, none granting write.
const VALID_ACL: [(u16, u16, u32); 6] = [
    (USER_OBJ, 7, UNDEFINED_ID),
    (USER, 5, OTHER),
    (GROUP_OBJ, 5, UNDEFINED_ID),
    (GROUP, 5, OTHER),
    (MASK, 5, UNDEFINED_ID),
    (OTHER_ENTRY, 5, UNDEFINED_ID),
];

/// [`VALID_ACL`], edited.
fn edited(edit: impl FnOnce(&mut Vec<(u16, u16, u32)>)) -> Vec<u8> {
    let mut entries = VALID_ACL.to_vec();
    edit(&mut entries);
    acl(&entries)
}

/// Scenario: Put ACL values that aren't a whole, valid ACL on an otherwise passing 0755 directory, and on the export in a 0700 directory: too short for a header, a header alone, version 1 or 3, an entry cut short or a stray byte after the last, an entry after the others entry, a lone others entry, a tag outside the six, permission bits outside read, write and execute (on a named entry and on others), a named user or group with no id, an owner, owning-group, mask or others entry with one, a missing owner, owning-group, others or (with named entries) mask entry, a second owner, owning-group, mask or others entry, the same named user or group twice, and entries out of the kernel's order. Each reads as no ACL and refuses as malformed where it is; the valid ACL they were made from passes in both places.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_malformed_acls_refuse() {
    let wrong_version = |version: u8| {
        let mut value = acl(&VALID_ACL);
        value[0] = version;
        value
    };
    let mut cases: Vec<(String, Vec<u8>)> = vec![
        ("no bytes".into(), vec![]),
        ("shorter than its header".into(), vec![2, 0]),
        ("a header alone".into(), acl(&[])),
        ("version 1".into(), wrong_version(1)),
        ("version 3".into(), wrong_version(3)),
        ("an entry cut short".into(), {
            let mut value = acl(&VALID_ACL);
            value.pop();
            value
        }),
        ("a stray byte after the last entry".into(), {
            let mut value = acl(&VALID_ACL);
            value.push(0);
            value
        }),
        (
            "a named user after others".into(),
            edited(|e| e.push((USER, 4, ME))),
        ),
        (
            "a lone others entry".into(),
            acl(&[(OTHER_ENTRY, 6, UNDEFINED_ID)]),
        ),
        (
            "a named user with no id".into(),
            edited(|e| e[1].2 = UNDEFINED_ID),
        ),
        (
            "a named group with no id".into(),
            edited(|e| e[3].2 = UNDEFINED_ID),
        ),
        (
            "no owner".into(),
            edited(|e| {
                e.remove(0);
            }),
        ),
        (
            "no owning group".into(),
            edited(|e| {
                e.remove(2);
            }),
        ),
        (
            "no mask with named entries".into(),
            edited(|e| {
                e.remove(4);
            }),
        ),
        (
            "no others".into(),
            edited(|e| {
                e.remove(5);
            }),
        ),
        ("two owners".into(), edited(|e| e.insert(0, e[0]))),
        ("two owning groups".into(), edited(|e| e.insert(2, e[2]))),
        ("two masks".into(), edited(|e| e.insert(4, e[4]))),
        ("two others".into(), edited(|e| e.push(e[5]))),
        (
            "the same named user twice".into(),
            edited(|e| e.insert(1, (USER, 4, OTHER))),
        ),
        (
            "the same named group twice".into(),
            edited(|e| e.insert(3, (GROUP, 4, OTHER))),
        ),
        ("the owning group first".into(), edited(|e| e.swap(0, 2))),
        (
            "a named user after the owning group".into(),
            edited(|e| e.swap(1, 2)),
        ),
        (
            "a named group before the owning group".into(),
            edited(|e| e.swap(2, 3)),
        ),
        (
            "the mask before a named group".into(),
            edited(|e| e.swap(3, 4)),
        ),
        ("others before the mask".into(), edited(|e| e.swap(4, 5))),
    ];
    for tag in [0x00, 0x03, 0x40, 0x8000, 0xFFFF] {
        cases.push((
            format!("tag {tag:#x}"),
            edited(|e| e.insert(1, (tag, 4, UNDEFINED_ID))),
        ));
    }
    for perm in [0o10, 0x100, 0x8000, 0xFFFF] {
        cases.push((
            format!("permissions {perm:#x} on a named user"),
            edited(|e| e[1].1 = perm),
        ));
        cases.push((
            format!("permissions {perm:#x} on others"),
            edited(|e| e[5].1 = perm),
        ));
    }
    for (index, id) in [(0, ROOT), (2, ME), (4, ROOT), (5, OTHER)] {
        cases.push((
            format!("entry {index} carrying id {id}"),
            edited(|e| e[index].2 = id),
        ));
    }

    let base = || {
        let mut tree = Tree::standard();
        tree.dir("/home/me/d", ME, 0o755);
        tree.file("/home/me/d/prompt.md", ME, 0o600);
        tree.dir("/home/me/p", ME, 0o700);
        tree.file("/home/me/p/prompt.md", ME, 0o600);
        tree
    };
    let mut tree = base();
    tree.set_xattr("/home/me/d", ACL, Some(acl(&VALID_ACL)));
    tree.set_xattr("/home/me/p/prompt.md", ACL, Some(acl(&VALID_ACL)));
    assert!(check(&tree, "/home/me/d/prompt.md", ME).is_ok());
    assert!(check(&tree, "/home/me/p/prompt.md", ME).is_ok());

    for (label, value) in cases {
        assert_eq!(acl_grants(&value), None, "{label}");
        let mut tree = base();
        tree.set_xattr("/home/me/d", ACL, Some(value.clone()));
        assert_eq!(
            refused(&tree, "/home/me/d/prompt.md", ME),
            refusal("/home/me/d", Cause::AclMalformed),
            "{label} on the directory"
        );
        let mut tree = base();
        tree.set_xattr("/home/me/p/prompt.md", ACL, Some(value));
        assert_eq!(
            refused(&tree, "/home/me/p/prompt.md", ME),
            refusal("/home/me/p/prompt.md", Cause::AclMalformed),
            "{label} on the export"
        );
    }
}

/// Scenario: Put the export's directory, a directory above it, `/` or the export alone on a filesystem outside the allow-list: NFS, CIFS, SMB2, 9p, FUSE, overlayfs, ZFS and Ceph. Each is refused at the first such component, an allow-listed directory reached through one included.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_filesystems_outside_the_allow_list_refuse() {
    let outside = [
        0x6969,
        0xFF53_4D42,
        0xFE53_4D42,
        0x0102_1997,
        0x6573_5546,
        0x794C_7630,
        0x2FC1_2FC1,
        0x00C3_6400,
    ];
    let base = || {
        let mut tree = Tree::standard();
        tree.dir("/home/me/a", ME, 0o755);
        tree.dir("/home/me/a/b", ME, 0o700);
        tree.file("/home/me/a/b/prompt.md", ME, 0o600);
        tree
    };
    let path = "/home/me/a/b/prompt.md";
    for f_type in outside {
        let mut tree = base();
        tree.set_fs("/home/me/a/b", f_type);
        assert_eq!(
            refused(&tree, path, ME),
            refusal("/home/me/a/b", Cause::Filesystem { f_type })
        );

        let mut tree = base();
        tree.set_fs("/home/me/a", f_type);
        assert_eq!(
            refused(&tree, path, ME),
            refusal("/home/me/a", Cause::Filesystem { f_type })
        );

        let mut tree = base();
        tree.set_fs("/", f_type);
        assert_eq!(
            refused(&tree, path, ME),
            refusal("/", Cause::Filesystem { f_type })
        );

        let mut tree = base();
        tree.set_fs(path, f_type);
        assert_eq!(
            refused(&tree, path, ME),
            refusal(path, Cause::Filesystem { f_type })
        );
        assert_eq!(refused(&tree, path, ME).remedy(ME), Remedy::Relocate);
    }
}

/// Scenario: The export itself: a 0666 or 0664 file in a 0755 directory, a 0660 file in a 0750 directory, a file with two links, root's file, a FIFO, a directory, a symlink to a good file, a missing file, a file where a directory belongs, a relative path and paths that name no file. Each refuses with its own cause; a 0664 file in a 0700 directory passes.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_the_export_itself() {
    let mut tree = Tree::standard();
    tree.dir("/home/me/open", ME, 0o755);
    tree.dir("/home/me/group", ME, 0o750);
    tree.dir("/home/me/private", ME, 0o700);
    let cases: [(&str, u32, u32, Cause); 4] = [
        (
            "/home/me/open/w.md",
            ME,
            0o666,
            Cause::FileWritable { mode: 0o666 },
        ),
        (
            "/home/me/open/g.md",
            ME,
            0o664,
            Cause::FileWritable { mode: 0o664 },
        ),
        (
            "/home/me/group/g.md",
            ME,
            0o660,
            Cause::FileWritable { mode: 0o660 },
        ),
        (
            "/home/me/open/root.md",
            ROOT,
            0o600,
            Cause::FileNotYours { uid: ROOT },
        ),
    ];
    for (path, uid, mode, cause) in cases {
        tree.file(path, uid, mode);
        assert_eq!(refused(&tree, path, ME), refusal(path, cause));
    }
    let private = tree.file("/home/me/private/g.md", ME, 0o664);
    assert_eq!(check(&tree, "/home/me/private/g.md", ME), Ok(private));

    let linked = tree.file("/home/me/private/two.md", ME, 0o600);
    tree.nodes[linked].nlink = 2;
    assert_eq!(
        refused(&tree, "/home/me/private/two.md", ME),
        refusal("/home/me/private/two.md", Cause::Links { nlink: 2 })
    );

    tree.add("/home/me/private/fifo", ME, 0o600, Shape::Fifo);
    assert_eq!(
        refused(&tree, "/home/me/private/fifo", ME),
        refusal("/home/me/private/fifo", Cause::NotARegularFile)
    );
    tree.dir("/home/me/private/sub", ME, 0o700);
    assert_eq!(
        refused(&tree, "/home/me/private/sub", ME),
        refusal("/home/me/private/sub", Cause::NotARegularFile)
    );
    tree.link("/home/me/private/ln.md", ME, "g.md");
    assert_eq!(
        refused(&tree, "/home/me/private/ln.md", ME),
        refusal("/home/me/private/ln.md", Cause::Symlink)
    );
    assert_eq!(
        refused(&tree, "/home/me/private/missing.md", ME),
        refusal(
            "/home/me/private/missing.md",
            Cause::Io {
                op: Op::Open,
                errno: Some(ENOENT)
            }
        )
    );
    assert_eq!(
        refused(&tree, "/home/me/private/g.md/x", ME),
        refusal("/home/me/private/g.md", Cause::NotADirectory)
    );
    assert_eq!(
        refused(&tree, "home/me/private/g.md", ME),
        refusal("home/me/private/g.md", Cause::NotAbsolute)
    );
    for path in ["/", "/home/me/.."] {
        assert_eq!(refused(&tree, path, ME), refusal(path, Cause::NoFileName));
    }
}

/// Scenario: Symlinks on the way: one inside `/tmp`, one in the home directory whose target passes through a group-writable directory, another account's link, a loop, a chain of links one past the limit and one at it, a link with an empty target, and `..` at `/`. The first five refuse as the rule says, the chain at the limit and the `..` pass.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_symlinks() {
    let mut tree = Tree::standard();
    tree.dir("/tmp/u", ME, 0o700);
    let in_tmp = tree.file("/tmp/u/prompt.md", ME, 0o600);
    tree.link("/tmp/ln", ME, "/tmp/u");
    assert_eq!(
        refused(&tree, "/tmp/ln/prompt.md", ME),
        refusal("/tmp/ln", Cause::Symlink)
    );
    tree.link("/tmp/root-ln", ROOT, "/tmp/u");
    assert_eq!(
        refused(&tree, "/tmp/root-ln/prompt.md", ME),
        refusal("/tmp/root-ln", Cause::Symlink)
    );

    tree.dir("/srv", ROOT, 0o755);
    tree.dir("/srv/team", ROOT, 0o775);
    tree.dir("/srv/team/u", ME, 0o700);
    tree.file("/srv/team/u/prompt.md", ME, 0o600);
    tree.link("/home/me/work", ME, "/srv/team/u");
    assert_eq!(
        refused(&tree, "/home/me/work/prompt.md", ME),
        refusal(
            "/srv/team",
            Cause::Writable {
                mode: 0o775,
                uid: ROOT
            }
        )
    );

    tree.link("/home/me/theirs", OTHER, "/tmp/u");
    assert_eq!(
        refused(&tree, "/home/me/theirs/prompt.md", ME),
        refusal("/home/me/theirs", Cause::ForeignOwner { uid: OTHER })
    );

    tree.link("/home/me/a", ME, "b");
    tree.link("/home/me/b", ME, "a");
    assert_eq!(
        refused(&tree, "/home/me/a/prompt.md", ME).cause,
        Cause::TooManySymlinks
    );

    tree.dir("/home/me/chain", ME, 0o700);
    tree.dir("/home/me/chain/end", ME, 0o700);
    let end = tree.file("/home/me/chain/end/prompt.md", ME, 0o600);
    tree.link(&format!("/home/me/chain/l{MAX_SYMLINK_HOPS}"), ME, "end");
    for hop in (1..MAX_SYMLINK_HOPS).rev() {
        tree.link(
            &format!("/home/me/chain/l{hop}"),
            ME,
            &format!("l{}", hop + 1),
        );
    }
    assert_eq!(check(&tree, "/home/me/chain/l1/prompt.md", ME), Ok(end));
    tree.link("/home/me/chain/l0", ME, "l1");
    assert_eq!(
        refused(&tree, "/home/me/chain/l0/prompt.md", ME),
        refusal(
            &format!("/home/me/chain/l{MAX_SYMLINK_HOPS}"),
            Cause::TooManySymlinks
        )
    );

    tree.link("/home/me/empty", ME, "");
    assert_eq!(
        refused(&tree, "/home/me/empty/prompt.md", ME),
        refusal("/home/me/empty", Cause::Symlink)
    );

    tree.link("/home/me/to-tmp", ME, "../../tmp/u");
    assert_eq!(check(&tree, "/home/me/to-tmp/prompt.md", ME), Ok(in_tmp));
    assert_eq!(check(&tree, "/../../tmp/u/prompt.md", ME), Ok(in_tmp));
}

/// Scenario: A syscall fails on the way: `/` can't be opened, a directory can't be opened, stat, statfs or an attribute read fails; and a directory or the export stats as another kind than it opened as, as if replaced in between. Each refuses at that component, naming the syscall and its errno, or the kind.
#[spec("inbox/probe/005")]
#[test]
fn probe_005_io_errors_refuse() {
    let base = || {
        let mut tree = Tree::standard();
        tree.dir("/home/me/d", ME, 0o700);
        tree.file("/home/me/d/prompt.md", ME, 0o600);
        tree
    };
    let path = "/home/me/d/prompt.md";
    let mut tree = base();
    tree.root_fails = Some(EIO);
    assert_eq!(
        refused(&tree, path, ME),
        refusal(
            "/",
            Cause::Io {
                op: Op::Open,
                errno: Some(EIO)
            }
        )
    );
    for (at, op) in [
        ("/home/me/d", Op::Open),
        ("/home/me/d", Op::Stat),
        ("/home/me/d", Op::FsType),
        (path, Op::Stat),
        (path, Op::FsType),
        (path, Op::Xattr),
    ] {
        let mut tree = base();
        tree.fail(at, op, EIO);
        assert_eq!(
            refused(&tree, path, ME),
            refusal(
                at,
                Cause::Io {
                    op,
                    errno: Some(EIO)
                }
            ),
            "{at} {op:?}"
        );
    }

    let mut tree = base();
    let d = tree.find("/home/me/d");
    tree.nodes[d].stat_kind = Some(Kind::File);
    assert_eq!(
        refused(&tree, path, ME),
        refusal("/home/me/d", Cause::NotADirectory)
    );
    let mut tree = base();
    let file = tree.find(path);
    tree.nodes[file].stat_kind = Some(Kind::Symlink);
    assert_eq!(
        refused(&tree, path, ME),
        refusal(path, Cause::NotARegularFile)
    );
}

/// The real filesystem, through the Linux view, under a fresh directory in
/// `/tmp`.
///
/// On a host whose `/` or `/tmp` is outside the allow-list (a container's
/// overlayfs root, the RWX sandbox's among them), every real walk stops at
/// that refusal: the tests then check only that refusal, the positives print
/// `SKIP-POSITIVE`, and what the walk does on an allowed filesystem past `/`
/// is covered there by the scripted cases above alone (and M47n, run apart).
#[cfg(target_os = "linux")]
mod real {
    use std::ffi::CString;
    use std::fs;
    use std::io::Read as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::Path;

    use dot_agent_deck::composer_probe::path_rule::{
        ALLOWED_FILESYSTEMS, Cause, FsView as _, Op, Refusal, Remedy, Xattr, verify_export,
    };
    use dot_agent_deck::platform::fsperm::fs_view::LinuxFsView;
    use spec::spec;

    use super::{GROUP_OBJ, MASK, OTHER_ENTRY, UNDEFINED_ID, USER, USER_OBJ, acl};

    fn uid() -> u32 {
        // SAFETY: `geteuid` has no preconditions.
        unsafe { libc::geteuid() }
    }

    // `f_type`'s width varies by architecture.
    #[allow(clippy::unnecessary_cast)]
    fn statfs_type(path: &Path) -> u32 {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `statfs` is plain data, so all-zero is a valid value.
        let mut info: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: a NUL-terminated path and a writable `statfs`.
        assert_eq!(unsafe { libc::statfs(path.as_ptr(), &mut info) }, 0);
        info.f_type as u32
    }

    /// Where this host refuses before the walk reaches the test's own
    /// directory: `/` or `/tmp` on a filesystem outside the allow-list (a
    /// container's overlayfs root), read by `statfs(2)` on the path, apart
    /// from the rule.
    fn host_refusal() -> Option<Refusal> {
        ["/", "/tmp"].into_iter().find_map(|at| {
            let f_type = statfs_type(Path::new(at));
            (!ALLOWED_FILESYSTEMS
                .iter()
                .any(|&(allowed, _)| allowed == f_type))
            .then(|| Refusal {
                at: at.into(),
                cause: Cause::Filesystem { f_type },
            })
        })
    }

    fn verify(path: &Path) -> Result<std::os::fd::OwnedFd, Refusal> {
        verify_export(&mut LinuxFsView, path, uid()).map(|verified| verified.file)
    }

    /// `expected` on a host whose `/` and `/tmp` are allow-listed, and the
    /// host's own refusal otherwise.
    fn expect_refusal(path: &Path, at: &Path, cause: Cause) {
        let expected = host_refusal().unwrap_or(Refusal {
            at: at.to_owned(),
            cause,
        });
        assert_eq!(verify(path).err(), Some(expected), "{}", path.display());
    }

    fn expect_pass(path: &Path, contents: &str) {
        match (verify(path), host_refusal()) {
            (Ok(fd), None) => {
                let mut read = String::new();
                fs::File::from(fd).read_to_string(&mut read).unwrap();
                assert_eq!(read, contents);
            }
            (Err(refusal), Some(host)) => {
                eprintln!(
                    "SKIP-POSITIVE: this host's {} isn't allow-listed",
                    host.at.display()
                );
                assert_eq!(refusal, host);
            }
            (seen, host) => panic!("{}: {seen:?}, host refusal {host:?}", path.display()),
        }
    }

    fn private_tmp() -> tempfile::TempDir {
        let tmp = fs::metadata("/tmp").unwrap();
        assert_eq!(
            tmp.permissions().mode() & 0o7777,
            0o1777,
            "/tmp isn't 01777"
        );
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        chmod(dir.path(), 0o700);
        dir
    }

    fn write(path: &Path, mode: u32) {
        fs::write(path, "the box\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Scenario: On the real filesystem, through the Linux view: the real `/tmp`, a private 0700 directory in it, and in that Claude's 0664 export and a 0600 one; then a safe symlink to a sibling private directory. Each passes, and the descriptor returned reads the file's contents. On a host whose `/` or `/tmp` is outside the allow-list, each is refused there instead.
    #[spec("inbox/probe/005")]
    #[test]
    fn probe_005_the_real_tmp_and_a_private_directory() {
        let dir = private_tmp();
        let claude = dir.path().join("claude.md");
        write(&claude, 0o664);
        expect_pass(&claude, "the box\n");
        let codex = dir.path().join("codex.md");
        write(&codex, 0o600);
        expect_pass(&codex, "the box\n");

        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        chmod(&real, 0o700);
        write(&real.join("prompt.md"), 0o600);
        symlink("real", dir.path().join("ln")).unwrap();
        expect_pass(&dir.path().join("ln/prompt.md"), "the box\n");
    }

    /// Scenario: On the real filesystem: a `CODEX_HOME` whose `editor` directory is 0775, as a umask of 002 makes it, holds at that directory with the `chmod g-w` remedy and passes once the bit is removed; a 0666 export in a 0755 directory, a FIFO, a file with a second hard link, a symlink in `/tmp`, a symlink loop, and an ACL on a 0755 directory whose named entry (for the user's own uid) carries write behind a read-and-execute mask each refuse, the last on the raw named write the mask hides.
    #[spec("inbox/probe/005")]
    #[test]
    fn probe_005_real_refusals_and_option_c() {
        let dir = private_tmp();
        let editor = dir.path().join("codex-home/editor");
        fs::create_dir_all(&editor).unwrap();
        chmod(&dir.path().join("codex-home"), 0o700);
        chmod(&editor, 0o775);
        let export = editor.join("prompt.md");
        write(&export, 0o600);
        expect_refusal(
            &export,
            &editor,
            Cause::Writable {
                mode: 0o775,
                uid: uid(),
            },
        );
        if host_refusal().is_none() {
            let held = verify(&export).unwrap_err();
            assert_eq!(held.remedy(uid()), Remedy::RemoveWrite { bits: 0o020 });
        }
        chmod(&editor, 0o755);
        expect_pass(&export, "the box\n");

        let open = dir.path().join("open");
        fs::create_dir(&open).unwrap();
        chmod(&open, 0o755);
        let writable = open.join("w.md");
        write(&writable, 0o666);
        expect_refusal(&writable, &writable, Cause::FileWritable { mode: 0o666 });

        let fifo = dir.path().join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        expect_refusal(&fifo, &fifo, Cause::NotARegularFile);

        let once = dir.path().join("once.md");
        write(&once, 0o600);
        fs::hard_link(&once, dir.path().join("twice.md")).unwrap();
        expect_refusal(&once, &once, Cause::Links { nlink: 2 });

        let in_tmp = Path::new("/tmp").join(format!(
            "{}-ln",
            dir.path().file_name().unwrap().to_str().unwrap()
        ));
        symlink(dir.path(), &in_tmp).unwrap();
        let through = in_tmp.join("codex-home/editor/prompt.md");
        expect_refusal(&through, &in_tmp, Cause::Symlink);
        fs::remove_file(&in_tmp).unwrap();

        symlink("loop-b", dir.path().join("loop-a")).unwrap();
        symlink("loop-a", dir.path().join("loop-b")).unwrap();
        let looped = dir.path().join("loop-a/prompt.md");
        if host_refusal().is_none() {
            assert_eq!(verify(&looped).unwrap_err().cause, Cause::TooManySymlinks);
        }

        let acl_dir = dir.path().join("acl");
        fs::create_dir(&acl_dir).unwrap();
        chmod(&acl_dir, 0o755);
        write(&acl_dir.join("prompt.md"), 0o600);
        // A named entry for the user's own uid, which every user namespace
        // maps; the rule refuses any named write grant.
        let value = acl(&[
            (USER_OBJ, 7, UNDEFINED_ID),
            (USER, 7, uid()),
            (GROUP_OBJ, 5, UNDEFINED_ID),
            (MASK, 5, UNDEFINED_ID),
            (OTHER_ENTRY, 5, UNDEFINED_ID),
        ]);
        let expected = match set_acl(&acl_dir, &value) {
            Ok(()) => {
                assert_eq!(
                    fs::metadata(&acl_dir).unwrap().permissions().mode() & 0o777,
                    0o755,
                    "the mask kept the group bits read-only"
                );
                Cause::AclWrite
            }
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP), "{error}");
                Cause::AclUnsupported
            }
        };
        expect_refusal(&acl_dir.join("prompt.md"), &acl_dir, expected);
    }

    /// Set `path`'s POSIX access ACL to `value`.
    fn set_acl(path: &Path, value: &[u8]) -> std::io::Result<()> {
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path and name, and a value of its length.
        let set = unsafe {
            libc::setxattr(
                name.as_ptr(),
                c"system.posix_acl_access".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if set == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// The `nobody` account's uid, when it exists and isn't the user's.
    fn nobody() -> Option<u32> {
        // SAFETY: a NUL-terminated name; the entry is read at once, before any
        // other `getpw*` call could overwrite it.
        let entry = unsafe { libc::getpwnam(c"nobody".as_ptr()) };
        // SAFETY: a non-null entry from `getpwnam`.
        let found = (!entry.is_null()).then(|| unsafe { (*entry).pw_uid });
        found.filter(|&found| found != uid())
    }

    /// Scenario: On the real filesystem, an ACL on a 0755 directory grants another existing account (`nobody`) read, write and execute behind a mask that lets it through, so that account can in effect write there: the directory's group bits show the mask (0775), and the export in it refuses at the directory as group-writable with the `chmod g-w` remedy. After `chmod g-w` the mask hides the grant (read back from the attribute) and it still refuses, on the raw named write; with the grant cut to read and execute it passes. An export given the same effective grant (0660 by its mask) in a 0755 directory refuses as writable. Where the filesystem has no POSIX ACLs or this user namespace doesn't map `nobody`, the test prints `SKIP-FOREIGN-ACL` and asserts nothing more; on a host whose `/` or `/tmp` is outside the allow-list, each walk is refused there instead.
    #[spec("inbox/probe/005")]
    #[test]
    fn probe_005_a_real_acl_grant_to_another_account() {
        let Some(nobody) = nobody() else {
            eprintln!("SKIP-FOREIGN-ACL: no `nobody` account apart from this user");
            return;
        };
        let grant = |named: u16, mask: u16| {
            acl(&[
                (USER_OBJ, 7, UNDEFINED_ID),
                (USER, named, nobody),
                (GROUP_OBJ, 5, UNDEFINED_ID),
                (MASK, mask, UNDEFINED_ID),
                (OTHER_ENTRY, 5, UNDEFINED_ID),
            ])
        };
        let dir = private_tmp();
        let shared = dir.path().join("shared");
        fs::create_dir(&shared).unwrap();
        chmod(&shared, 0o755);
        let export = shared.join("prompt.md");
        write(&export, 0o600);
        if let Err(error) = set_acl(&shared, &grant(7, 7)) {
            assert!(
                matches!(error.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EINVAL)),
                "{error}"
            );
            eprintln!("SKIP-FOREIGN-ACL: setting an ACL for uid {nobody}: {error}");
            return;
        }
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o775,
            "the mask shows in the group bits"
        );
        expect_refusal(
            &export,
            &shared,
            Cause::Writable {
                mode: 0o775,
                uid: uid(),
            },
        );
        if host_refusal().is_none() {
            let held = verify(&export).unwrap_err();
            assert_eq!(held.remedy(uid()), Remedy::RemoveWrite { bits: 0o020 });
        }

        chmod(&shared, 0o755);
        let fd = std::os::fd::OwnedFd::from(fs::File::open(&shared).unwrap());
        assert_eq!(
            LinuxFsView.xattr(&fd, "system.posix_acl_access").unwrap(),
            Xattr::Present(grant(7, 5)),
            "chmod masked the grant and kept it"
        );
        expect_refusal(&export, &shared, Cause::AclWrite);
        set_acl(&shared, &grant(5, 5)).unwrap();
        expect_pass(&export, "the box\n");

        let granted = shared.join("granted.md");
        write(&granted, 0o600);
        set_acl(
            &granted,
            &acl(&[
                (USER_OBJ, 6, UNDEFINED_ID),
                (USER, 6, nobody),
                (GROUP_OBJ, 0, UNDEFINED_ID),
                (MASK, 6, UNDEFINED_ID),
                (OTHER_ENTRY, 0, UNDEFINED_ID),
            ]),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&granted).unwrap().permissions().mode() & 0o777,
            0o660
        );
        expect_refusal(&granted, &granted, Cause::FileWritable { mode: 0o660 });
    }

    const DESCRIPTOR_ZERO_CHILD: &str = "DAD_PROBE_005_DESCRIPTOR_ZERO_CHILD";

    /// Scenario: The Linux view takes a descriptor of 0 as an open, not an error: the test runs itself again alone in a child process, which writes a 0600 export in a private directory under `/tmp`, closes its standard input so the first descriptor the walk opens (`/`) is 0, and checks the export passes and reads back (or, on a host whose `/` or `/tmp` is outside the allow-list, is refused there by its filesystem, not by a failed open).
    #[spec("inbox/probe/005")]
    #[test]
    fn probe_005_descriptor_zero_is_an_open() {
        if std::env::var_os(DESCRIPTOR_ZERO_CHILD).is_some() {
            let dir = private_tmp();
            let export = dir.path().join("prompt.md");
            write(&export, 0o600);
            // SAFETY: this process runs this test alone (below), and nothing
            // in it reads standard input.
            assert_eq!(unsafe { libc::close(0) }, 0);
            // SAFETY: `fcntl` on a closed descriptor only fails.
            assert_eq!(unsafe { libc::fcntl(0, libc::F_GETFD) }, -1, "0 is free");
            expect_pass(&export, "the box\n");
            println!("descriptor zero: checked");
            return;
        }
        let name = "real::probe_005_descriptor_zero_is_an_open";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(DESCRIPTOR_ZERO_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        // libtest exits 0 when the filter matches nothing.
        assert!(
            stdout.contains("descriptor zero: checked"),
            "{stdout}\n{stderr}"
        );
    }

    /// Scenario: On the real filesystem, through the Linux view: a symlink as the export itself refuses even though its target would pass; a FIFO where a directory belongs refuses without blocking; an export and a directory on its path that the user can't open (mode 000) refuse naming the open's `EACCES` (not checked when running as root); and the view reads a 2,000-byte extended attribute whole, reports a missing one as absent, and reports one the filesystem doesn't support as unsupported.
    #[spec("inbox/probe/005")]
    #[test]
    fn probe_005_the_linux_view_opens_without_following() {
        let dir = private_tmp();
        let target = dir.path().join("target.md");
        write(&target, 0o600);
        let link = dir.path().join("link.md");
        symlink("target.md", &link).unwrap();
        expect_refusal(&link, &link, Cause::Symlink);

        // A FIFO where a directory belongs: opened as a directory, it
        // refuses at once rather than blocking for a writer.
        let fifo = dir.path().join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        expect_refusal(&fifo.join("prompt.md"), &fifo, Cause::NotADirectory);

        if uid() != 0 {
            let denied = Cause::Io {
                op: Op::Open,
                errno: Some(libc::EACCES),
            };
            let locked = dir.path().join("locked.md");
            write(&locked, 0o000);
            expect_refusal(&locked, &locked, denied.clone());
            let shut = dir.path().join("shut");
            fs::create_dir(&shut).unwrap();
            write(&shut.join("prompt.md"), 0o600);
            chmod(&shut, 0o000);
            expect_refusal(&shut.join("prompt.md"), &shut, denied);
            chmod(&shut, 0o700);
        }

        let mut view = LinuxFsView;
        let fd = std::os::fd::OwnedFd::from(fs::File::open(&target).unwrap());
        assert_eq!(view.xattr(&fd, "user.dad-absent").unwrap(), Xattr::Absent);
        let value: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        let name = CString::new(target.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path and name, and a value of its length.
        let set = unsafe {
            libc::setxattr(
                name.as_ptr(),
                c"user.dad-big".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        let read = view.xattr(&fd, "user.dad-big").unwrap();
        if set == 0 {
            assert_eq!(read, Xattr::Present(value));
        } else {
            let error = std::io::Error::last_os_error();
            assert_eq!(error.raw_os_error(), Some(libc::EOPNOTSUPP), "{error}");
            assert_eq!(read, Xattr::Unsupported);
        }
    }
}
