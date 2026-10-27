//! PRD #1858: where the deck finds each run's usage record, and which paths
//! it hands a reader at all.
//!
//! Every record is read beneath a [`TrustedDir`]: an agent's configuration
//! directory, Codex's `sessions` directory or the deck's own usage
//! directory, each resolved once — so a configuration root the user
//! symlinked elsewhere is followed there and only there — then kept open.
//! The path beneath it is built here from values the deck holds — the
//! session id and working directory an agent's hook reported, the deck's own
//! run id — or, for Codex, admitted here from the path Codex's hook reported:
//! under the agent's own sessions directory, named as the agent names it. A
//! reader opens it relative to the kept directory, refusing a symlink in any
//! component of it (on Unix), and refuses anything but a regular file of this
//! user's ([`super::JsonlReader`]).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;

use super::Ident;

/// The most project directories [`claude_transcript`] looks through for a
/// session it cannot find where its directory name says.
pub const MAX_PROJECT_DIRS: usize = 4096;

/// The longest directory name Claude Code derives from a working directory
/// without shortening it; a longer one is looked for by session id instead.
const MAX_PLAIN_PROJECT_NAME: usize = 200;

/// A directory the deck reads an agent's records beneath, opened once.
///
/// [`TrustedDir::open`] resolves its path once, following any symlink in it,
/// and requires a directory owned by the user the deck runs as. On Unix it
/// then keeps the directory open, and every later open beneath it is made
/// relative to that descriptor with no symlink followed in any component
/// ([`crate::telemetry::collector::disk::fd::open_beneath_at`]: `openat2`
/// with `RESOLVE_BENEATH` and `RESOLVE_NO_SYMLINKS` on Linux, one
/// `O_NOFOLLOW` component at a time elsewhere), so a directory inside it
/// replaced by a symlink later is refused rather than followed. Elsewhere
/// it keeps the resolved path and opens through it, which a directory
/// swapped in between two opens would redirect.
#[derive(Clone)]
pub struct TrustedDir {
    #[cfg(unix)]
    fd: Arc<std::os::fd::OwnedFd>,
    path: PathBuf,
}

impl std::fmt::Debug for TrustedDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A path is not shown: it can name the user.
        f.debug_struct("TrustedDir").finish_non_exhaustive()
    }
}

#[cfg(unix)]
const OPEN_FILE: libc::c_int =
    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
#[cfg(unix)]
const OPEN_DIR: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

impl TrustedDir {
    /// `root`, resolved and opened as described on [`TrustedDir`]; `None`
    /// when it is not there, is not a directory, or is another user's.
    pub fn open(root: &Path) -> Option<TrustedDir> {
        let path = std::fs::canonicalize(root).ok()?;
        #[cfg(unix)]
        {
            let fd = crate::telemetry::collector::disk::fd::open_dir_no_symlinks(&path).ok()?;
            let dir = TrustedDir {
                fd: Arc::new(fd),
                path,
            };
            dir.owned_directory().then_some(dir)
        }
        #[cfg(not(unix))]
        {
            std::fs::symlink_metadata(&path)
                .is_ok_and(|m| m.is_dir())
                .then_some(TrustedDir { path })
        }
    }

    /// The resolved path it was opened at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory `rel` beneath this one, opened as this one was.
    pub fn subdir(&self, rel: &Path) -> Option<TrustedDir> {
        #[cfg(unix)]
        {
            let fd = self.open_raw(rel, OPEN_DIR, 0).ok()?;
            let dir = TrustedDir {
                fd: Arc::new(fd),
                path: self.path.join(rel),
            };
            dir.owned_directory().then_some(dir)
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(rel);
            std::fs::symlink_metadata(&path)
                .is_ok_and(|m| m.is_dir())
                .then_some(TrustedDir { path })
        }
    }

    /// The file `rel` beneath this one, opened for reading without blocking
    /// on a FIFO and, on Unix, with no symlink followed in any component.
    pub fn open_file(&self, rel: &Path) -> io::Result<File> {
        #[cfg(unix)]
        {
            self.open_raw(rel, OPEN_FILE, 0).map(File::from)
        }
        #[cfg(not(unix))]
        {
            File::open(self.path.join(plain(rel)?))
        }
    }

    /// Whether `rel` beneath this one opens as a regular file.
    pub fn has_file(&self, rel: &Path) -> bool {
        self.open_file(rel)
            .and_then(|f| f.metadata())
            .is_ok_and(|m| m.is_file())
    }

    /// The names of the entries of this directory, at most `limit`; empty
    /// when it cannot be read.
    pub fn entry_names(&self, limit: usize) -> Vec<OsString> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            let Ok(dir) = self.fd.try_clone().map(File::from) else {
                return Vec::new();
            };
            crate::telemetry::launch::dir_entry_names(&dir)
                .into_iter()
                .take(limit)
                .map(|name| OsString::from_vec(name.into_bytes()))
                .collect()
        }
        #[cfg(not(unix))]
        {
            std::fs::read_dir(&self.path)
                .map(|entries| {
                    entries
                        .flatten()
                        .take(limit)
                        .map(|e| e.file_name())
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    /// Remove the entry `name` of this directory, never what a symlink there
    /// points at.
    pub fn remove(&self, name: &OsStr) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::ffi::OsStrExt as _;
            if let Ok(name) = std::ffi::CString::new(name.as_bytes()) {
                // SAFETY: `name` is NUL-terminated and the descriptor is open;
                // `unlinkat` removes a name, never what a symlink points at.
                unsafe { libc::unlinkat(self.fd.as_raw_fd(), name.as_ptr(), 0) };
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(self.path.join(name));
        }
    }

    /// Rename the entry `from` of this directory to `to`, both names in it,
    /// unless an entry `to` is there already: then it fails, with
    /// `AlreadyExists`, and replaces nothing. On Linux by `renameat2` with
    /// `RENAME_NOREPLACE`; where the file system does not support that, and
    /// elsewhere, by `linkat`, which does not replace a name either, then
    /// removing `from`. Entries are renamed, never what a symlink points at.
    #[cfg(unix)]
    pub(crate) fn rename_new(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::ffi::OsStrExt as _;
        let name = |n: &OsStr| {
            std::ffi::CString::new(n.as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
        };
        let (from, to) = (name(from)?, name(to)?);
        let fd = self.fd.as_raw_fd();
        #[cfg(target_os = "linux")]
        {
            // SAFETY: both names are NUL-terminated and the descriptor is
            // open.
            let renamed = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    fd,
                    from.as_ptr(),
                    fd,
                    to.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if renamed == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
                return Err(error);
            }
        }
        // SAFETY: both names are NUL-terminated and the descriptor is open;
        // without `AT_SYMLINK_FOLLOW`, `linkat` links the entry itself.
        if unsafe { libc::linkat(fd, from.as_ptr(), fd, to.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        unsafe { libc::unlinkat(fd, from.as_ptr(), 0) };
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn open_raw(
        &self,
        rel: &Path,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> io::Result<std::os::fd::OwnedFd> {
        use std::os::fd::AsFd as _;
        crate::telemetry::collector::disk::fd::open_beneath_at(
            self.fd.as_fd(),
            plain(rel)?,
            flags,
            mode,
        )
    }

    /// The open directory's own metadata.
    #[cfg(unix)]
    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata> {
        File::from(self.fd.try_clone()?).metadata()
    }

    /// Make the directory `name` in this one with `mode`, unless something
    /// of that name is there already.
    #[cfg(unix)]
    pub(crate) fn make_dir(&self, name: &OsStr, mode: libc::mode_t) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `name` is NUL-terminated and the descriptor is open.
        if unsafe { libc::mkdirat(self.fd.as_raw_fd(), name.as_ptr(), mode) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::AlreadyExists => Ok(()),
            _ => Err(error),
        }
    }

    /// Set the open directory's permission bits to `mode`.
    #[cfg(unix)]
    pub(crate) fn set_mode(&self, mode: libc::mode_t) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        // SAFETY: the descriptor is open.
        if unsafe { libc::fchmod(self.fd.as_raw_fd(), mode) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(unix)]
    fn owned_directory(&self) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        self.metadata()
            .is_ok_and(|m| m.is_dir() && m.uid() == super::effective_uid())
    }
}

/// `rel` when it is one or more plain names: no root, `.` or `..`.
fn plain(rel: &Path) -> io::Result<&Path> {
    let ok = rel.components().next().is_some()
        && rel.components().all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(rel)
    } else {
        Err(io::Error::from(io::ErrorKind::InvalidInput))
    }
}

/// An absolute path from an environment value, or `None`.
fn absolute(value: Option<&OsStr>) -> Option<PathBuf> {
    let path = PathBuf::from(value.filter(|v| !v.is_empty())?);
    path.is_absolute().then_some(path)
}

/// Claude Code's configuration directory: `CLAUDE_CONFIG_DIR`, else
/// `.claude` in `HOME`, each only when absolute.
pub fn claude_config_dir(
    claude_config_dir: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    absolute(claude_config_dir).or_else(|| absolute(home).map(|h| h.join(".claude")))
}

/// Codex's home: `CODEX_HOME`, else `.codex` in `HOME`, each only when
/// absolute.
pub fn codex_home(codex_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    absolute(codex_home).or_else(|| absolute(home).map(|h| h.join(".codex")))
}

/// Whether `id` can name a Claude Code session's transcript: 1 to 128 ASCII
/// letters, digits, `-` and `_`, so it cannot climb out of a directory.
pub fn admissible_session_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The directory name Claude Code keeps a working directory's transcripts
/// under: every character but an ASCII letter or digit becomes `-`. `None`
/// for a name longer than Claude Code keeps unshortened.
pub fn claude_project_dir_name(cwd: &str) -> Option<String> {
    let name: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    (name.len() <= MAX_PLAIN_PROJECT_NAME).then_some(name)
}

/// The transcript of Claude Code session `session_id`, started in `cwd`,
/// beneath Claude Code's configuration directory `config`:
/// `projects/<cwd's directory name>/<session_id>.jsonl`, the one place
/// [`claude_transcript_in_place`] looks; or, when that is not there (a
/// working directory whose name Claude Code shortened, or one the hook
/// reported differently) and `search` is set, the file of that name in any of
/// the first [`MAX_PROJECT_DIRS`] project directories. Returned as the path
/// beneath `config`. `None` for a session id that could name another file,
/// or when no such regular file opens; a project directory that is a
/// symlink, or a transcript that is one, does not open.
pub fn claude_transcript(
    config: &TrustedDir,
    cwd: Option<&str>,
    session_id: &str,
    search: bool,
) -> Option<PathBuf> {
    if let Some(found) = claude_transcript_in_place(config, cwd, session_id) {
        return Some(found);
    }
    if !search || !admissible_session_id(session_id) {
        return None;
    }
    let file = format!("{session_id}.jsonl");
    let projects = Path::new("projects");
    config
        .subdir(projects)?
        .entry_names(MAX_PROJECT_DIRS)
        .into_iter()
        .map(|name| projects.join(name).join(&file))
        .find(|rel| config.has_file(rel))
}

/// [`claude_transcript`] without its search: only the file the working
/// directory names.
pub fn claude_transcript_in_place(
    config: &TrustedDir,
    cwd: Option<&str>,
    session_id: &str,
) -> Option<PathBuf> {
    if !admissible_session_id(session_id) {
        return None;
    }
    let rel = Path::new("projects")
        .join(claude_project_dir_name(cwd?)?)
        .join(format!("{session_id}.jsonl"));
    config.has_file(&rel).then_some(rel)
}

/// The rollout path a Codex hook reported, admitted only when it is
/// absolute, has no `..`, names a file `rollout-*.jsonl`, lies beneath
/// `sessions`, Codex's sessions directory (given as `sessions_path` and
/// opened as `sessions`), and opens there as a regular file. Returns the
/// path beneath `sessions`. The reported path is taken beneath the sessions
/// directory as written, or as it resolves when it reaches it through a
/// symlink.
pub fn admit_codex_rollout(
    reported: &str,
    sessions_path: &Path,
    sessions: &TrustedDir,
) -> Option<PathBuf> {
    let path = Path::new(reported);
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    let beneath = |p: &Path| {
        p.strip_prefix(sessions_path)
            .or_else(|_| p.strip_prefix(sessions.path()))
            .ok()
            .map(Path::to_path_buf)
    };
    let rel = beneath(path).or_else(|| beneath(&std::fs::canonicalize(path).ok()?))?;
    let name = rel.file_name()?.to_str()?;
    (plain(&rel).is_ok()
        && name.starts_with("rollout-")
        && name.ends_with(".jsonl")
        && sessions.has_file(&rel))
    .then_some(rel)
}

/// The name of run `run_id`'s usage file in the deck's usage directory, for
/// the daemon holding lease `lease` ([`lease_file_name`]):
/// `<run_id>-<lease>.usage.jsonl`. The lease says which daemon owns the file,
/// so a daemon starting later removes only files whose lease it can take
/// ([`usage_file_lease`]). `None` for a run id that is not an identifier, or
/// that holds a `/` or starts with `.`, or a lease that is not one.
pub fn usage_file_name(run_id: &str, lease: &str) -> Option<String> {
    let id = Ident::new(run_id)?;
    (!id.as_str().contains('/') && !id.as_str().starts_with('.') && is_lease(lease.as_bytes()))
        .then(|| format!("{}-{lease}{USAGE_FILE_SUFFIX}", id.as_str()))
}

const USAGE_FILE_SUFFIX: &str = ".usage.jsonl";

/// The suffix of a daemon's lease file, and of one it is still making.
const LEASE_SUFFIX: &str = ".lease";
const NEW_LEASE_SUFFIX: &str = ".lease-new";

/// A lease: 32 lowercase hexadecimal digits, as [`crate::telemetry::span::new_run_id`] makes them.
fn is_lease(lease: &[u8]) -> bool {
    lease.len() == 32 && lease.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The owning daemon's lease of a usage file named as [`usage_file_name`]
/// names them; `None` for any other name.
pub fn usage_file_lease(name: &[u8]) -> Option<&[u8]> {
    let stem = name.strip_suffix(USAGE_FILE_SUFFIX.as_bytes())?;
    let dash = stem.iter().rposition(|b| *b == b'-')?;
    let (run, lease) = (&stem[..dash], &stem[dash + 1..]);
    let run_ok = std::str::from_utf8(run).is_ok_and(|r| Ident::new(r).is_some())
        && !run.is_empty()
        && !run.starts_with(b".");
    (run_ok && is_lease(lease)).then_some(lease)
}

/// The name of the file a daemon holding lease `lease` keeps locked while it
/// runs, and the name it makes it under before it is locked.
pub fn lease_file_name(lease: &str, made: bool) -> String {
    format!(
        "{lease}{}",
        if made { LEASE_SUFFIX } else { NEW_LEASE_SUFFIX }
    )
}

/// The lease of a lease file named as [`lease_file_name`] names a made one;
/// `None` for any other name, one still being made included.
pub fn lease_of_file(name: &[u8]) -> Option<&[u8]> {
    let lease = name.strip_suffix(LEASE_SUFFIX.as_bytes())?;
    is_lease(lease).then_some(lease)
}

/// The lease of a lease file named as [`lease_file_name`] names one still
/// being made; `None` for any other name, a made one included.
pub fn lease_being_made(name: &[u8]) -> Option<&[u8]> {
    let lease = name.strip_suffix(NEW_LEASE_SUFFIX.as_bytes())?;
    is_lease(lease).then_some(lease)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn config_dirs_come_from_the_variable_then_home_and_must_be_absolute() {
        let os = |s: &str| Some(OsStr::new(s).to_owned());
        assert_eq!(
            claude_config_dir(os("/c").as_deref(), os("/h").as_deref()),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            claude_config_dir(None, os("/h").as_deref()),
            Some(PathBuf::from("/h/.claude"))
        );
        assert_eq!(
            claude_config_dir(os("rel").as_deref(), os("rel").as_deref()),
            None
        );
        assert_eq!(
            codex_home(None, os("/h").as_deref()),
            Some(PathBuf::from("/h/.codex"))
        );
        assert_eq!(codex_home(os("").as_deref(), None), None);
    }

    #[test]
    fn a_project_directory_name_replaces_everything_but_letters_and_digits() {
        assert_eq!(
            claude_project_dir_name("/home/u/code/dot-agent.deck").as_deref(),
            Some("-home-u-code-dot-agent-deck")
        );
        assert_eq!(claude_project_dir_name(&"/a".repeat(101)), None);
    }

    fn trusted(path: &Path) -> TrustedDir {
        TrustedDir::open(path).expect("a directory of this user's")
    }

    /// The transcript is found where the directory name says, else (when
    /// searching) by its session id in another project directory; a session
    /// id that could name another file is refused, and neither a symlinked
    /// transcript nor a symlinked project directory is taken.
    #[test]
    fn a_transcript_is_found_by_directory_then_by_session_id() {
        let dir = crate::test_temp::tempdir().unwrap();
        let config = dir.path();
        let id = "0f1e2d3c-aaaa-bbbb-cccc-1234567890ab";
        let here = config.join("projects").join("-work-repo");
        std::fs::create_dir_all(&here).unwrap();
        File::create(here.join(format!("{id}.jsonl"))).unwrap();
        let root = trusted(config);
        let rel = Path::new("projects/-work-repo").join(format!("{id}.jsonl"));
        assert_eq!(
            claude_transcript(&root, Some("/work/repo"), id, false),
            Some(rel.clone())
        );
        assert_eq!(
            claude_transcript(&root, Some("/elsewhere"), id, false),
            None
        );
        assert_eq!(
            claude_transcript(&root, Some("/elsewhere"), id, true),
            Some(rel),
            "found by session id when the directory differs"
        );
        assert_eq!(
            claude_transcript(&root, Some("/work/repo"), "../x", true),
            None
        );
        assert_eq!(
            claude_transcript(&root, Some("/work/repo"), "a/b", true),
            None
        );
        assert_eq!(claude_transcript(&root, None, "missing", true), None);
        #[cfg(unix)]
        {
            let link_id = "linked";
            std::os::unix::fs::symlink(
                here.join(format!("{id}.jsonl")),
                here.join(format!("{link_id}.jsonl")),
            )
            .unwrap();
            assert_eq!(
                claude_transcript(&root, Some("/work/repo"), link_id, true),
                None
            );
        }
    }

    /// A project directory that is a symlink is not followed, neither where
    /// the working directory names it nor in the search, even when what it
    /// points at holds the session's transcript.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_project_directory_is_not_followed() {
        let dir = crate::test_temp::tempdir().unwrap();
        let config = dir.path().join("config");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(config.join("projects")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let id = "s-linked-project";
        File::create(outside.join(format!("{id}.jsonl"))).unwrap();
        std::os::unix::fs::symlink(&outside, config.join("projects/-work-repo")).unwrap();
        let root = trusted(&config);
        assert_eq!(claude_transcript(&root, Some("/work/repo"), id, true), None);
    }

    /// A configuration root the user symlinked elsewhere is followed once,
    /// where the deck resolves it, and read beneath what it resolved to.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_configuration_root_is_resolved_and_kept() {
        let dir = crate::test_temp::tempdir().unwrap();
        let real = dir.path().join("dotfiles-claude");
        let here = real.join("projects/-w");
        std::fs::create_dir_all(&here).unwrap();
        File::create(here.join("s-1.jsonl")).unwrap();
        let link = dir.path().join(".claude");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let root = trusted(&link);
        assert_eq!(root.path(), std::fs::canonicalize(&real).unwrap());
        assert!(claude_transcript(&root, Some("/w"), "s-1", false).is_some());
    }

    /// A rollout is admitted only under the Codex home's `sessions`, by its
    /// own name, as its path beneath it.
    #[test]
    fn a_rollout_is_admitted_only_under_the_sessions_root() {
        let dir = crate::test_temp::tempdir().unwrap();
        let home = dir.path().join(".codex");
        let day = home.join("sessions/2026/10/26");
        std::fs::create_dir_all(&day).unwrap();
        let rollout = day.join("rollout-2026-10-26T10-00-00-abc.jsonl");
        File::create(&rollout).unwrap();
        let other = dir.path().join("rollout-elsewhere.jsonl");
        File::create(&other).unwrap();
        let wrong_name = day.join("notes.jsonl");
        File::create(&wrong_name).unwrap();
        let sessions_path = home.join("sessions");
        let sessions = trusted(&sessions_path);
        let admit = |p: &str| admit_codex_rollout(p, &sessions_path, &sessions);
        assert_eq!(
            admit(rollout.to_str().unwrap()),
            Some(PathBuf::from(
                "2026/10/26/rollout-2026-10-26T10-00-00-abc.jsonl"
            ))
        );
        assert_eq!(admit(other.to_str().unwrap()), None);
        assert_eq!(admit(wrong_name.to_str().unwrap()), None);
        let climbing = format!(
            "{}/../26/{}",
            day.display(),
            "rollout-2026-10-26T10-00-00-abc.jsonl"
        );
        assert_eq!(admit(&climbing), None);
        assert_eq!(admit("rollout-x.jsonl"), None);
        #[cfg(unix)]
        {
            let link = day.join("rollout-link.jsonl");
            std::os::unix::fs::symlink(&other, &link).unwrap();
            assert_eq!(
                admit(link.to_str().unwrap()),
                None,
                "a link out of the root does not open beneath it"
            );
        }
    }

    /// Once admitted, a rollout is opened beneath the kept sessions
    /// directory each time: an ancestor replaced by a symlink to a directory
    /// outside it is refused, not followed to a file of the same name.
    #[cfg(unix)]
    #[test]
    fn an_ancestor_swapped_after_admission_is_refused() {
        let dir = crate::test_temp::tempdir().unwrap();
        let sessions_path = dir.path().join(".codex/sessions");
        let day = sessions_path.join("2026/10/26");
        std::fs::create_dir_all(&day).unwrap();
        let name = "rollout-swap.jsonl";
        File::create(day.join(name)).unwrap();
        let sessions = trusted(&sessions_path);
        let rel = admit_codex_rollout(day.join(name).to_str().unwrap(), &sessions_path, &sessions)
            .unwrap();
        assert!(sessions.open_file(&rel).is_ok());
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        File::create(outside.join(name)).unwrap();
        std::fs::rename(&day, dir.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &day).unwrap();
        assert!(sessions.open_file(&rel).is_err());
        assert!(!sessions.has_file(&rel));
    }

    #[test]
    fn a_usage_file_is_named_by_its_run_and_daemons_lease() {
        let lease = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            usage_file_name("run-1a2b", lease).as_deref(),
            Some("run-1a2b-0123456789abcdef0123456789abcdef.usage.jsonl")
        );
        assert_eq!(usage_file_name("../x", lease), None);
        assert_eq!(usage_file_name("a/b", lease), None);
        assert_eq!(usage_file_name("a b", lease), None);
        assert_eq!(usage_file_name(".hidden", lease), None);
        assert_eq!(usage_file_name("run", "42"), None);
        let name = usage_file_name("0123abcd", lease).unwrap();
        assert_eq!(usage_file_lease(name.as_bytes()), Some(lease.as_bytes()));
        for other in [
            &b"run-.usage.jsonl"[..],
            b"run-12.usage.jsonl",
            b"run-0123456789abcdef0123456789abcdef.usage.json",
            b"-0123456789abcdef0123456789abcdef.usage.jsonl",
            b"run-0123456789ABCDEF0123456789abcdef.usage.jsonl",
            b"notes.txt",
        ] {
            assert_eq!(usage_file_lease(other), None, "{other:?}");
        }
        let (made, making) = (lease_file_name(lease, true), lease_file_name(lease, false));
        assert_eq!(lease_of_file(made.as_bytes()), Some(lease.as_bytes()));
        assert_eq!(lease_being_made(making.as_bytes()), Some(lease.as_bytes()));
        // A lease being made is not one cleanup takes as a lease, nor the
        // other way round.
        assert_eq!(lease_of_file(making.as_bytes()), None);
        assert_eq!(lease_being_made(made.as_bytes()), None);
        assert_eq!(lease_of_file(b"42.lease"), None);
        assert_eq!(lease_being_made(b"42.lease-new"), None);
        assert_eq!(lease_of_file(b"notes.txt"), None);
    }
}
