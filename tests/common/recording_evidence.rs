//! Preserve completed recordings before the flat, reel-compatible files are
//! invalidated at launch. Read Claude metadata from the isolated session's
//! transcript, never from requested defaults in settings.

use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const RETAINED_RUNS: usize = 10;
const FAILURE_PROTECTION_SECS: u64 = 7 * 24 * 60 * 60;
const PENDING_MAX_AGE_SECS: u64 = 60 * 60;

/// Serialize complete publications, archive-plus-clear and retention for one
/// test. The file stays in place: unlinking a lock would create two lock inodes.
fn with_recording_lock<T>(dir: &Path, action: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    std::fs::create_dir_all(dir)?;
    require_directory(dir)?;
    let lock = open_lock(&dir.join(".recording.lock"))?;
    lock.lock()?;
    action()
}

pub(crate) fn publish_recording(
    dir: &Path,
    write: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    with_recording_lock(dir, || {
        archive_and_clear_locked(dir)?;
        write()
    })
}

pub(crate) fn archive_before_discard(dir: &Path) -> std::io::Result<()> {
    with_recording_lock(dir, || archive_and_clear_locked(dir))
}

fn archive_and_clear_locked(dir: &Path) -> io::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    archive_recording(dir, now.as_secs(), now.subsec_nanos())?;
    // Publication succeeded. Invalidate the flat view before optional cleanup,
    // so a failed prune cannot cause the next attempt to archive it again.
    for name in super::RECORDING_ARTIFACTS {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if let Err(error) =
        prune_recordings_locked(dir, now.as_secs(), RETAINED_RUNS, FAILURE_PROTECTION_SECS)
    {
        eprintln!(
            "[tui-harness] evidence archived; retention cleanup at {} failed: {error}",
            dir.display()
        );
    }
    Ok(())
}

fn archive_recording(dir: &Path, now: u64, nanos: u32) -> std::io::Result<()> {
    let mut artifacts = Vec::new();
    for name in super::RECORDING_ARTIFACTS {
        match open_beneath(dir, Path::new(name), false) {
            Ok(file) => artifacts.push((name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", dir.join(name).display()),
                ));
            }
        }
    }
    if artifacts.is_empty() {
        return Ok(());
    }
    let runs = dir.join("runs");
    std::fs::create_dir_all(&runs)?;
    require_directory(&runs)?;
    let pending = tempfile::Builder::new()
        .prefix(".pending-")
        .tempdir_in(&runs)?;
    let writer = open_lock(&pending.path().join(".writer.lock"))?;
    writer.lock()?;
    for (name, mut source) in artifacts {
        let mut destination = File::create(pending.path().join(name))?;
        io::copy(&mut source, &mut destination)?;
    }
    let suffix = pending.path().file_name().unwrap().to_string_lossy();
    let archived = runs.join(format!("run-{now:020}-{nanos:09}-{suffix}"));
    // Publish the archive only after its copies are complete. On error the
    // original flat files survive, and TempDir removes the partial copy.
    std::fs::rename(pending.path(), archived)?;
    Ok(())
}

/// A soft count cap: recent failed (or incomplete) runs are protected even
/// when that temporarily leaves more than `keep` archives. Expired failures
/// compete with successes by recording time, oldest first.
pub(crate) fn prune_recordings(
    dir: &Path,
    now: u64,
    keep: usize,
    protect_secs: u64,
) -> std::io::Result<()> {
    with_recording_lock(dir, || {
        prune_recordings_locked(dir, now, keep, protect_secs)
    })
}

fn prune_recordings_locked(dir: &Path, now: u64, keep: usize, protect_secs: u64) -> io::Result<()> {
    let runs_dir = dir.join("runs");
    match require_directory(&runs_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let entries = match std::fs::read_dir(dir.join("runs")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_dir() && name.starts_with(".pending-") {
            reap_pending(&entry.path(), now)?;
            continue;
        }
        if !entry.file_type()?.is_dir() || !name.starts_with("run-") {
            continue;
        }
        let Some(archived_at) = name.split('-').nth(1).and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        let provenance = open_beneath(&entry.path(), Path::new("provenance.json"), false)
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(64 * 1024).read_to_end(&mut bytes)?;
                Ok(bytes)
            })
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let recorded_at = provenance
            .as_ref()
            .and_then(|v| v["recorded_at_unix"].as_u64())
            .unwrap_or(archived_at);
        let passed = provenance
            .as_ref()
            .is_some_and(|v| v["outcome"] == "passed");
        let protected = !passed && now.saturating_sub(recorded_at) < protect_secs;
        runs.push((recorded_at, entry.path(), protected));
    }
    runs.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let mut excess = runs.len().saturating_sub(keep);
    for (_, path, protected) in runs {
        if excess == 0 {
            break;
        }
        if !protected {
            match std::fs::remove_dir_all(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            excess -= 1;
        }
    }
    Ok(())
}

fn reap_pending(path: &Path, now: u64) -> io::Result<()> {
    let modified = std::fs::symlink_metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if now.saturating_sub(modified) < PENDING_MAX_AGE_SECS {
        return Ok(());
    }
    // Inspect age before creating the marker: creating it changes directory
    // mtime. A killed writer releases its kernel lock even without Drop.
    let writer = open_lock(&path.join(".writer.lock"))?;
    if dot_agent_deck::platform::lock::try_lock_file(&writer)? {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn open_lock(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("recording lock is not a regular file"));
    }
    Ok(file)
}

/// Extract metadata rather than persisting the transcript (which contains
/// prompts, tool output and possibly credentials). Unknown observations remain
/// null; settings and command-line model aliases are not evidence.
pub(crate) fn claude_session_provenance(home: &Path) -> Vec<Value> {
    // Anchor the walk at the harness root, including overridden HOME paths.
    // Neither HOME's ancestors below it nor .claude/projects may be links.
    let Ok(root) = session_root(home) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut entries_left = 4096;
    let mut collection_truncated = false;
    session_files(
        &root,
        Path::new(".claude/projects"),
        0,
        &mut entries_left,
        &mut collection_truncated,
        &mut files,
    );
    // Choose after enumeration, rather than taking an arbitrary read_dir prefix.
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    collection_truncated |= files.len() > 256;
    files.truncate(256);
    let mut budget = 16 * 1024 * 1024;
    let mut results = Vec::new();
    for mut transcript in files {
        if budget == 0 {
            collection_truncated = true;
            break;
        }
        let limit = transcript.length.min(1024 * 1024).min(budget);
        budget -= limit;
        let mut result = empty_session(&transcript.path);
        collect_session_tail(&mut transcript.file, transcript.length, limit, &mut result);
        results.push(result);
    }
    for result in &mut results {
        result["collection_truncated"] = json!(collection_truncated);
    }
    results
}

pub(crate) fn has_claude_sessions(home: &Path) -> bool {
    session_root(home).is_ok()
}

fn session_root(home: &Path) -> io::Result<File> {
    let sandbox = super::harness_temp_root();
    let relative = home.strip_prefix(sandbox).map_err(io::Error::other)?;
    open_beneath(sandbox, &relative.join(".claude/projects"), true)
}

pub(crate) fn collect_session_tail(
    mut file: impl Read + Seek,
    length: u64,
    limit: u64,
    result: &mut Value,
) {
    let limit = length.min(limit);
    let skipped_prefix = length > limit;
    result["truncated"] = json!(skipped_prefix);
    match file.seek(SeekFrom::Start(length - limit)) {
        Ok(_) => {
            // Take is below BufReader, so prefetch and a growing file cannot
            // read past the length snapshot or this run's budget.
            let reader = io::BufReader::new(file.take(limit));
            collect_session_records(reader, skipped_prefix, result);
        }
        Err(error) => {
            result["read_error"] = json!(error.kind().to_string());
            result["truncated"] = json!(true);
        }
    }
}

fn empty_session(path: &Path) -> Value {
    json!({"transcript": path.to_string_lossy(), "session_id": null,
        "permissionMode": null, "permission_modes": [], "version": null,
        "models": [], "model": null, "truncated": false})
}

/// Bounded JSONL records; an oversized record is drained without retaining its
/// bytes. I/O errors and partial EOF records explicitly mark a gap.
pub(crate) fn collect_session_records(
    mut reader: impl BufRead,
    mut skip_first: bool,
    result: &mut Value,
) {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok([]) => {
                if !skip_first
                    && !oversized
                    && !line.is_empty()
                    && !collect_session_record(&line, result)
                {
                    result["truncated"] = json!(true);
                }
                break;
            }
            Ok(chunk) => chunk,
            Err(error) => {
                result["read_error"] = json!(error.kind().to_string());
                result["truncated"] = json!(true);
                break;
            }
        };
        let end = chunk.iter().position(|&b| b == b'\n').map(|i| i + 1);
        let consumed = end.unwrap_or(chunk.len());
        if !skip_first && !oversized {
            if line.len() + consumed <= 64 * 1024 {
                line.extend_from_slice(&chunk[..consumed]);
            } else {
                oversized = true;
                result["truncated"] = json!(true);
                line.clear();
            }
        }
        reader.consume(consumed);
        if end.is_none() {
            continue;
        }
        if !skip_first && !oversized {
            collect_session_record(&line, result);
        }
        skip_first = false;
        oversized = false;
        line.clear();
    }
}

fn collect_session_record(line: &[u8], result: &mut Value) -> bool {
    let Ok(record) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    if !record.is_object() {
        return false;
    }
    for (source, target) in [
        ("sessionId", "session_id"),
        ("version", "version"),
        ("permissionMode", "permissionMode"),
    ] {
        if let Some(value) = record[source].as_str() {
            result[target] = json!(value);
        }
    }
    if let Some(mode) = record["permissionMode"].as_str() {
        push_distinct(&mut result["permission_modes"], mode);
    }
    if let Some(model) = record["message"]["model"].as_str() {
        result["model"] = json!(model);
        push_distinct(&mut result["models"], model);
    }
    true
}

fn push_distinct(values: &mut Value, value: &str) {
    let values = values.as_array_mut().unwrap();
    if !values.iter().any(|v| v == value) {
        values.push(json!(value));
    }
}

struct Transcript {
    path: PathBuf,
    file: File,
    length: u64,
    modified: std::time::SystemTime,
}

fn session_files(
    dir: &File,
    path: &Path,
    depth: usize,
    entries_left: &mut usize,
    truncated: &mut bool,
    files: &mut Vec<Transcript>,
) {
    if depth > 6 {
        *truncated = true;
        return;
    }
    let Ok(names) = directory_names(dir, entries_left, truncated) else {
        *truncated = true;
        return;
    };
    for name in names {
        let relative = Path::new(&name);
        if let Ok(child) = open_child(dir, relative, true) {
            session_files(
                &child,
                &path.join(&name),
                depth + 1,
                entries_left,
                truncated,
                files,
            );
        } else if relative.extension().is_some_and(|e| e == "jsonl")
            && let Ok(file) = open_child(dir, relative, false)
            && let Ok(metadata) = file.metadata()
        {
            files.push(Transcript {
                path: path.join(name),
                file,
                length: metadata.len(),
                modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            });
            if files.len() > 256 {
                *truncated = true;
                // Keep descriptors for the newest candidates, bounding open
                // files as well as the eventual output and parse budget.
                files.sort_by(|a, b| {
                    b.modified
                        .cmp(&a.modified)
                        .then_with(|| a.path.cmp(&b.path))
                });
                files.pop();
            }
        }
    }
}

/// Read logs only below the harness-managed root, using the same pinned,
/// no-follow traversal as transcripts. Overrides can point elsewhere for a
/// test, but evidence collection must not import those outside files.
pub(crate) fn sandbox_log(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let root = super::harness_temp_root();
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(None);
    };
    let Ok(mut file) = open_beneath(root, relative, false) else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

fn open_beneath(root: &Path, path: &Path, directory: bool) -> io::Result<File> {
    let mut parent = open_root(root)?;
    let components: Vec<_> = path.components().collect();
    for (i, component) in components.iter().enumerate() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => {
                parent = open_child(
                    &parent,
                    Path::new(name),
                    directory || i + 1 < components.len(),
                )?;
            }
            _ => return Err(io::Error::other("path leaves the evidence root")),
        }
    }
    Ok(parent)
}

#[cfg(unix)]
fn open_root(root: &Path) -> io::Result<File> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(root.as_os_str().as_bytes())?;
    super::open_dir_nofollow(None, &name)
}

#[cfg(unix)]
fn open_child(parent: &File, name: &Path, directory: bool) -> io::Result<File> {
    use std::os::unix::{
        ffi::OsStrExt,
        io::{AsRawFd, FromRawFd},
    };
    let name = std::ffi::CString::new(name.as_os_str().as_bytes())?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent owns a live descriptor; name is a single NUL-terminated
    // component. The returned descriptor is adopted exactly once below.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor owned by this call.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(io::Error::other("not a regular evidence file or directory"));
    }
    Ok(file)
}

#[cfg(unix)]
fn directory_names(
    dir: &File,
    remaining: &mut usize,
    truncated: &mut bool,
) -> io::Result<Vec<std::ffi::OsString>> {
    use std::os::unix::{ffi::OsStringExt, io::IntoRawFd};
    let readable = open_child(dir, Path::new("."), true)?;
    let fd = readable.into_raw_fd();
    // SAFETY: fd is an owned directory descriptor. fdopendir takes ownership
    // on success; on failure we close it ourselves.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(error);
    }
    struct DirStream(*mut libc::DIR);
    impl Drop for DirStream {
        fn drop(&mut self) {
            // SAFETY: this guard exclusively owns the successful fdopendir.
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = DirStream(stream);
    let mut names = Vec::new();
    loop {
        if *remaining == 0 {
            *truncated = true;
            break;
        }
        // SAFETY: stream stays live; the returned entry is copied before the
        // next readdir. d_name is NUL-terminated by libc.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            break;
        }
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        *remaining -= 1;
        names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
    }
    names.sort();
    Ok(names)
}

#[cfg(windows)]
fn handle_path(file: &File) -> io::Result<PathBuf> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    let mut buffer = vec![0u16; 32768];
    // SAFETY: file owns the handle, and buffer is writable for the supplied
    // number of UTF-16 units. The function retains neither argument.
    let length = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.len() {
        return Err(io::Error::other("evidence path is too long"));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_wide(
        &buffer[..length],
    )))
}

#[cfg(windows)]
fn open_windows(path: &Path, directory: bool) -> io::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let file = File::options()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(io::Error::other("not a regular evidence file or directory"));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_root(root: &Path) -> io::Result<File> {
    open_windows(root, true)
}

#[cfg(windows)]
fn open_child(parent: &File, name: &Path, directory: bool) -> io::Result<File> {
    let parent_path = handle_path(parent)?;
    let expected = if name == Path::new(".") {
        parent_path.clone()
    } else {
        parent_path.join(name)
    };
    let file = open_windows(&expected, directory)?;
    // Validate the opened handle rather than trusting the checked pathname.
    if handle_path(&file)? != expected {
        return Err(io::Error::other("evidence path changed during open"));
    }
    Ok(file)
}

#[cfg(windows)]
fn directory_names(
    dir: &File,
    remaining: &mut usize,
    truncated: &mut bool,
) -> io::Result<Vec<std::ffi::OsString>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(handle_path(dir)?)? {
        if *remaining == 0 {
            *truncated = true;
            break;
        }
        *remaining -= 1;
        names.push(entry?.file_name());
    }
    names.sort();
    Ok(names)
}

fn require_directory(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.file_type().is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{} is not a regular recording directory",
            path.display()
        )))
    }
}
