//! Size-bounded reads for caller-supplied input.
//!
//! Issue #328. `std::fs::read_to_string` and `Read::read_to_string` are
//! unbounded in both directions that matter: they allocate however much the
//! source produces, and they wait however long it takes to produce it. That is
//! fine for a file this process wrote; it is not fine for a path or a stream
//! handed in on the command line, where the two pathological shapes are a file
//! that is enormous (or growing) and a target that is not a file at all — a
//! FIFO with no writer blocks forever, `/dev/zero` never ends.
//!
//! Two things live here, deliberately separated. [`read_capped`] is the
//! general primitive — read anything, stop at a byte cap, refuse rather than
//! truncate — and carries no policy of its own. [`read_task_input`] is the one
//! policy built on it: the `--task-file` reader, which additionally opens a
//! path once and refuses anything that is not a regular file, and whose error
//! wording names that flag because it is the only thing that calls it.
//!
//! [`crate::config::load_features_file`] applies the same shape inline with
//! different failure semantics (a bad `[features]` file keeps the previous
//! value and warns rather than erroring), which is why it is not expressed in
//! terms of these functions.

use std::io::Read;

/// Upper bound on the task/summary text accepted from `--task-file <path>` and
/// `--task-file -`.
///
/// A task is prose destined for an agent's prompt, so the cap only has to sit
/// far enough above "the longest brief anyone would actually write" that no
/// legitimate input can reach it. 1 MiB clears that by a wide margin in both
/// directions: this repository's largest PRD is ~117 KiB and a task file is a
/// task *description*, not a whole PRD; and 1 MiB of prose is roughly 250k
/// tokens, past the context window of the agents the text is being written
/// for. Anything above it is pathological rather than long, and gets a clear
/// refusal instead of an allocation.
pub const MAX_TASK_BYTES: u64 = 1024 * 1024;

/// Read `reader` to a `String`, refusing input larger than `max_bytes`.
///
/// `source` names the input in error messages ("task from stdin", "task file
/// '…'") and is expected to read as a lowercase noun phrase.
///
/// The reader is capped at `max_bytes + 1` rather than `max_bytes` so the two
/// outcomes stay distinguishable: input exactly at the limit is accepted, and
/// input past it is *detected* without being read — the extra byte is the
/// evidence, so an endless stream costs one megabyte and a refusal rather than
/// the machine's memory. Nothing is ever silently truncated.
pub fn read_capped(reader: impl Read, max_bytes: u64, source: &str) -> Result<String, String> {
    let mut buf = String::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_string(&mut buf)
        .map_err(|e| format!("failed to read {source}: {e}"))?;
    if buf.len() as u64 > max_bytes {
        return Err(over_limit(source, max_bytes));
    }
    Ok(buf)
}

/// Read the task/summary text for `--task-file <path>`, or from `stdin` when
/// `path` is `-`. Both are capped at [`MAX_TASK_BYTES`].
///
/// `-` deliberately keeps no file-type requirement — stdin is a pipe or a
/// terminal by design, and the cap is the whole of its protection. A `path`,
/// by contrast, must name a **regular file**, which is what the two shapes
/// this guards against are not: a FIFO with no writer never produces a byte,
/// and a character device such as `/dev/zero` never stops producing them.
pub fn read_task_input(path: &str, stdin: impl Read) -> Result<String, String> {
    if path == "-" {
        read_capped(stdin, MAX_TASK_BYTES, "task from stdin")
    } else {
        read_task_file(path)
    }
}

/// The path branch of [`read_task_input`], with two properties beyond
/// [`read_capped`]:
///
/// * **Opened once, judged from the open handle.** The type check reads
///   `File::metadata` (an `fstat` on the descriptor already held), not a second
///   `std::fs::metadata` on the path, so there is no window in which the thing
///   checked and the thing read can differ.
/// * **The open cannot hang.** On Unix the handle is opened `O_NONBLOCK`,
///   because a plain `open(2)` of a FIFO with no writer blocks *inside the
///   open* — before any check could run, which would reproduce the hang this
///   function exists to prevent. The flag is ignored for regular files, the
///   only kind that survives the check below.
///
/// Symlinks are followed deliberately (no `O_NOFOLLOW`): a task file reached
/// through a symlink is ordinary, and since the check is applied to the
/// resolved target, a symlink pointing at `/dev/zero` or a FIFO is refused
/// just the same. This matches [`crate::config::load_features_file`].
fn read_task_file(path: &str) -> Result<String, String> {
    let source = format!("task file '{path}'");
    let io_err = |e: std::io::Error| format!("failed to read {source}: {e}");

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        // Windows refuses `open` on a directory outright (`Access is denied`,
        // os error 5), so control never reaches the type check below and the
        // caller would see a bare OS error instead of the refusal this
        // function documents. Recover the documented message from the path so
        // the refusal reads the same on every platform.
        //
        // Consulting the path here cannot reintroduce the TOCTOU the open
        // handle exists to avoid: the open has already failed, so nothing is
        // read on this branch either way, and the only thing a race can change
        // is the wording of an error that is returned regardless.
        Err(e) => {
            return Err(if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
                not_a_regular_file(&source, "a directory")
            } else {
                io_err(e)
            });
        }
    };

    let metadata = file.metadata().map_err(io_err)?;
    if !metadata.is_file() {
        return Err(not_a_regular_file(
            &source,
            describe_file_type(&metadata.file_type()),
        ));
    }
    // Cheap and exact: refuse an oversized file by its recorded length rather
    // than by reading a megabyte of it first. `read_capped` still applies the
    // cap afterwards, which is what catches a file that grows past the limit
    // between this check and the read.
    if metadata.len() > MAX_TASK_BYTES {
        return Err(over_limit(&source, MAX_TASK_BYTES));
    }

    read_capped(file, MAX_TASK_BYTES, &source)
}

/// The not-a-regular-file refusal, shared so the type check and the Windows
/// open-failure recovery word it identically.
fn not_a_regular_file(source: &str, kind: &str) -> String {
    format!(
        "{source} is {kind}; --task-file needs a regular file (for a pipe, a process \
         substitution, or a terminal, pipe the text in and pass `--task-file -` instead)"
    )
}

/// The over-limit refusal, shared so the file and stdin paths word it
/// identically.
fn over_limit(source: &str, max_bytes: u64) -> String {
    format!(
        "{source} exceeds the {max_bytes}-byte limit; a task is prose, so shorten it and point \
         the agent at any bulk content by path instead of inlining it"
    )
}

/// Name a rejected file type in a way that tells the caller what they actually
/// pointed at. The generic fallback still completes the sentence in
/// [`read_task_file`]'s message.
fn describe_file_type(file_type: &std::fs::FileType) -> &'static str {
    if file_type.is_dir() {
        return "a directory";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if file_type.is_fifo() {
            return "a FIFO";
        }
        if file_type.is_socket() {
            return "a socket";
        }
        if file_type.is_char_device() {
            return "a character device";
        }
        if file_type.is_block_device() {
            return "a block device";
        }
    }
    "not a regular file"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` on a scratch thread and fail — rather than hang the tier — if it
    /// has not returned within five seconds. Every refusal here is guarding
    /// against an input that blocks forever, so "the call returned at all" is
    /// half of what these tests assert.
    fn within_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the bounded read must return promptly, not block");
        handle.join().expect("worker thread panicked");
        got
    }

    #[test]
    fn read_capped_accepts_input_exactly_at_the_limit() {
        let input = "x".repeat(16);
        let got = read_capped(input.as_bytes(), 16, "test input").expect("16 bytes under a 16 cap");
        assert_eq!(
            got, input,
            "input exactly at the cap must be accepted whole"
        );
    }

    #[test]
    fn read_capped_refuses_input_one_byte_over_the_limit() {
        let input = "x".repeat(17);
        let err = read_capped(input.as_bytes(), 16, "test input")
            .expect_err("17 bytes must not pass a 16-byte cap");
        assert!(
            err.contains("test input") && err.contains("exceeds the 16-byte limit"),
            "over-limit error should name the source and the cap: {err}"
        );
    }

    #[test]
    fn read_capped_refuses_an_endless_reader_instead_of_consuming_it() {
        // The `/dev/zero` shape at the reader level: a stream that never ends.
        // Unbounded, this allocates until the process dies.
        let err = within_timeout(|| {
            read_capped(std::io::repeat(b'z'), 4096, "endless input")
                .expect_err("an endless reader must be refused")
        });
        assert!(
            err.contains("exceeds the 4096-byte limit"),
            "endless input should be refused by the cap: {err}"
        );
    }

    #[test]
    fn regular_file_is_read_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("task.md");
        std::fs::write(&path, "line one\nline two\n").expect("write");
        let got =
            read_task_file(path.to_str().unwrap()).expect("a regular file under the cap must read");
        assert_eq!(got, "line one\nline two\n");
    }

    #[test]
    fn oversized_regular_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.md");
        std::fs::write(&path, "x".repeat(MAX_TASK_BYTES as usize + 1)).expect("write");
        let err = read_task_file(path.to_str().unwrap())
            .expect_err("a file over the cap must be refused");
        assert!(
            err.contains(&format!("exceeds the {MAX_TASK_BYTES}-byte limit"))
                && err.contains("huge.md"),
            "over-limit error should name the file and the cap: {err}"
        );
    }

    #[test]
    fn missing_file_error_names_the_path() {
        let err = read_task_file("/no/such/task-file.md").expect_err("a missing file must error");
        assert!(
            err.contains("failed to read task file") && err.contains("/no/such/task-file.md"),
            "missing-file error should name the path: {err}"
        );
    }

    #[test]
    fn directory_is_refused_as_not_a_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err =
            read_task_file(dir.path().to_str().unwrap()).expect_err("a directory must be refused");
        assert!(
            err.contains("is a directory") && err.contains("--task-file"),
            "directory error should say what it is and what is required: {err}"
        );
    }

    #[cfg(unix)]
    fn mkfifo_at(path: &std::path::Path) {
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("cstring");
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
        // call, and `mkfifo` only reads through it.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
    }

    /// The hang case. With no writer attached, a plain `open(2)` of this FIFO
    /// never returns — so this test asserts both that the read is refused and
    /// that it is refused *promptly*.
    #[cfg(unix)]
    #[test]
    fn fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("task.fifo");
        mkfifo_at(&path);
        let arg = path.to_str().unwrap().to_string();
        let err = within_timeout(move || read_task_file(&arg).expect_err("a FIFO must be refused"));
        assert!(
            err.contains("is a FIFO") && err.contains("--task-file -"),
            "FIFO error should name the type and point at the stdin alternative: {err}"
        );
    }

    /// A symlink to a FIFO is refused too: the type check reads the resolved
    /// target, so no-follow semantics are not needed to close this.
    #[cfg(unix)]
    #[test]
    fn symlink_to_fifo_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("real.fifo");
        mkfifo_at(&target);
        let link = dir.path().join("task.md");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let arg = link.to_str().unwrap().to_string();
        let err = within_timeout(move || {
            read_task_file(&arg).expect_err("a symlink to a FIFO must be refused")
        });
        assert!(
            err.contains("is a FIFO"),
            "the check must judge the resolved target: {err}"
        );
    }

    /// The endless-device case: `/dev/zero` is a character device, refused on
    /// its type before a single byte is read.
    #[cfg(unix)]
    #[test]
    fn character_device_is_refused() {
        if !std::path::Path::new("/dev/zero").exists() {
            return;
        }
        let err =
            within_timeout(|| read_task_file("/dev/zero").expect_err("/dev/zero must be refused"));
        assert!(
            err.contains("is a character device"),
            "device error should name the type: {err}"
        );
    }
}
