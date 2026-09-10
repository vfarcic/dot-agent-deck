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
//! The same two shapes reach the daemon over its **hook socket** (issue #903,
//! duplicate #319), which is why the async half lives here too: a caller-supplied
//! newline-delimited message is a stream that may never produce the newline it
//! is being read up to.
//!
//! Three things live here, deliberately separated. [`read_capped`] is the
//! general synchronous primitive — read anything, stop at a byte cap, refuse
//! rather than truncate — and carries no policy of its own. [`read_task_input`]
//! is the one policy built on it: the `--task-file` reader, which additionally
//! opens a path once and refuses anything that is not a regular file, and whose
//! error wording names that flag because it is the only thing that calls it.
//! [`read_capped_line`] is the async, line-oriented sibling of the primitive,
//! written for the daemon's hook socket and applying the identical
//! refuse-never-truncate rule to a `\n`-delimited message.
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

// ---------------------------------------------------------------------------
// The async, line-oriented sibling — the daemon's hook socket (issue #903,
// duplicate #319)
// ---------------------------------------------------------------------------

/// Upper bound on ONE newline-delimited message accepted on the daemon's hook
/// socket.
///
/// **Where the number comes from.** The largest message any producer *this
/// project ships* puts on this socket is a `delegate` or `work-done` whose
/// `task` field carries the brief or the completion report verbatim, and that
/// text has a codified cap already: [`MAX_TASK_BYTES`] (1 MiB), applied by
/// [`read_task_input`] on the CLI side. Everything else on the wire is small by
/// construction, and one sentence covers every agent because every bundled hook
/// adapter — Claude Code's `settings.json` entry, the Codex and Devin hook
/// configs, the OpenCode plugin — funnels its payload through
/// `dot-agent-deck hook`, which truncates `user_prompt` at
/// [`crate::prompt_delivery::USER_PROMPT_MAX_LEN`] (200 chars) and sharpens
/// `tool_detail` to a short label. The one field it copies through unbounded is
/// `metadata["bash_command"]`, and that is a shell command line.
///
/// 8 MiB is 8x that 1 MiB ceiling, which is the headroom the *encoding* needs:
/// the task rides inside a JSON string, and `serde_json` escapes `"`, `\`, and
/// the whitespace control characters to two bytes each — so a megabyte of
/// newline-heavy prose can arrive as two megabytes on the wire, with the
/// envelope and every other field on top. It is deliberately BELOW
/// `daemon_protocol::MAX_FRAME_LEN` (16 MiB, the attach socket's bound) for a
/// reason that does not apply there: the attach socket serves one framed
/// client, whereas this ceiling is multiplied by
/// [`MAX_CONCURRENT_HOOK_CONNECTIONS`](crate::daemon::MAX_CONCURRENT_HOOK_CONNECTIONS)
/// — the two bounds together are what put a finite number on the daemon's
/// worst-case hook-ingest footprint.
///
/// Note what this does NOT claim: a third-party producer writing raw
/// `AgentEvent` JSON to this socket is not bound by [`MAX_TASK_BYTES`] at all,
/// since it never goes through our CLI. The 8x is what makes the bound
/// uninteresting to such a producer too, not a proof that nothing can reach it.
pub const MAX_HOOK_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Why [`read_capped_line`] stopped short of returning a line.
#[derive(Debug)]
pub enum CappedLineError {
    /// `limit` bytes arrived with no `\n` among them. Refused, never
    /// truncated — what has been read is a *prefix* of a message nobody can
    /// complete, and the danger of truncating is not the lost bytes but that a
    /// prefix of a JSON object can parse: `serde` would happily fill an event
    /// from a half-read payload. The caller must drop the connection.
    TooLong {
        /// The cap that was exceeded, so the log line can name it.
        limit: usize,
        /// How long the line was already known to be at the moment of refusal:
        /// the bytes accumulated plus whatever the reader had ready, all of
        /// which is line content because no newline was among it. It can
        /// therefore exceed `limit` by up to one reader-buffer's worth — the
        /// refusal is what stops it growing further. Never the bytes
        /// themselves: they are producer-controlled, and a log is the wrong
        /// place to reproduce them.
        line_bytes: usize,
    },
    /// The read failed. This includes invalid UTF-8, which
    /// `tokio::io::Lines` also reports as `InvalidData` — so the byte-cap
    /// rewrite does not change how a non-UTF-8 payload is treated.
    Io(std::io::Error),
}

impl std::fmt::Display for CappedLineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong { limit, line_bytes } => write!(
                f,
                "line exceeds the {limit}-byte limit ({line_bytes} bytes seen with no newline)"
            ),
            Self::Io(e) => write!(f, "read failed: {e}"),
        }
    }
}

impl std::error::Error for CappedLineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TooLong { .. } => None,
            Self::Io(e) => Some(e),
        }
    }
}

/// Read one `\n`-delimited line from `reader`, refusing a line longer than
/// `max_bytes`.
///
/// The async counterpart of [`read_capped`], and the reason it exists:
/// `tokio::io::AsyncBufReadExt::lines` (and `read_until`) grow their buffer
/// until a newline arrives or the peer goes away, so a peer that sends bytes
/// and never a newline makes the reader allocate without limit. This resolves
/// the same three outcomes but with a ceiling:
///
/// * `Ok(Some(line))` — a complete line, its trailing `\n` (and a preceding
///   `\r`, matching `Lines`) removed. A line of exactly `max_bytes` is
///   ACCEPTED; the cap is inclusive, as in [`read_capped`].
/// * `Ok(None)` — the peer closed with nothing buffered. The ordinary end of
///   every fire-and-forget send.
/// * `Err(..)` — see [`CappedLineError`].
///
/// A final line with no trailing newline is returned as a line, exactly as
/// `Lines` does, so a producer that writes its JSON and closes without a
/// newline keeps working.
///
/// The buffer never grows past `max_bytes`: unlike [`read_capped`], which reads
/// one extra byte as its evidence, this one already knows a chunk without a
/// newline in it is all line, so it can refuse without buffering the overage.
/// The reader's own internal buffer (8 KiB by default) is the only thing held
/// on top.
pub async fn read_capped_line<R>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, CappedLineError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt as _;

    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(e) => return Err(CappedLineError::Io(e)),
        };
        if available.is_empty() {
            // EOF. A trailing partial line is still a line (`Lines` semantics);
            // nothing buffered means the peer simply closed.
            if buf.is_empty() {
                return Ok(None);
            }
            return finish(buf);
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(newline) => {
                // The line's length excludes the newline itself, so a line
                // ending exactly at the cap is within it.
                if buf.len() + newline > max_bytes {
                    return Err(CappedLineError::TooLong {
                        limit: max_bytes,
                        line_bytes: buf.len() + newline,
                    });
                }
                buf.extend_from_slice(&available[..newline]);
                reader.consume(newline + 1);
                return finish(buf);
            }
            None => {
                // Every byte in this chunk belongs to the line, so the line is
                // already at least this long — no newline can arrive to make
                // it shorter. Refuse now rather than buffer the overage.
                let chunk = available.len();
                if buf.len() + chunk > max_bytes {
                    return Err(CappedLineError::TooLong {
                        limit: max_bytes,
                        line_bytes: buf.len() + chunk,
                    });
                }
                buf.extend_from_slice(available);
                reader.consume(chunk);
            }
        }
    }
}

/// Turn an accumulated line's bytes into the `String` [`read_capped_line`]
/// returns, dropping a CRLF's `\r` the way `tokio::io::Lines` does and
/// reporting non-UTF-8 as `InvalidData` for the same reason.
fn finish(mut buf: Vec<u8>) -> Result<Option<String>, CappedLineError> {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    match String::from_utf8(buf) {
        Ok(line) => Ok(Some(line)),
        Err(e) => Err(CappedLineError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.utf8_error(),
        ))),
    }
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

    // -----------------------------------------------------------------------
    // `read_capped_line` — the daemon hook socket's bound (issues #903 / #319)
    // -----------------------------------------------------------------------
    //
    // These use small caps deliberately: they pin the *semantics* of the
    // reader, and the production constant is exercised against the real
    // `run_hook_loop` by `hooks/ingest/001` (`src/daemon.rs`). Splitting it
    // that way keeps the fast tier from moving 8 MiB per assertion while
    // still proving the shipped number is the one the daemon applies.

    /// Run `f` to completion or fail — rather than hang the tier — after five
    /// seconds. Every refusal below guards against a peer that never sends the
    /// newline being waited for, so "it returned at all" is half the claim.
    async fn line_within_timeout<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(5), f)
            .await
            .expect("the bounded line read must return promptly, not block")
    }

    /// A `BufReader` over an in-memory byte slice, the shape every test below
    /// reads from.
    fn line_reader(bytes: &[u8]) -> tokio::io::BufReader<std::io::Cursor<Vec<u8>>> {
        tokio::io::BufReader::new(std::io::Cursor::new(bytes.to_vec()))
    }

    #[tokio::test]
    async fn read_capped_line_accepts_a_line_exactly_at_the_limit() {
        let line = "x".repeat(16);
        let mut reader = line_reader(format!("{line}\n").as_bytes());
        let got = line_within_timeout(read_capped_line(&mut reader, 16))
            .await
            .expect("16 bytes under a 16-byte cap");
        assert_eq!(
            got.as_deref(),
            Some(line.as_str()),
            "a line exactly at the cap must be accepted whole — the cap is \
             inclusive, matching `read_capped`"
        );
    }

    #[tokio::test]
    async fn read_capped_line_refuses_a_line_one_byte_over_the_limit() {
        let mut reader = line_reader(format!("{}\n", "x".repeat(17)).as_bytes());
        let err = line_within_timeout(read_capped_line(&mut reader, 16))
            .await
            .expect_err("17 bytes must not pass a 16-byte cap");
        match err {
            CappedLineError::TooLong { limit, line_bytes } => {
                assert_eq!(limit, 16, "the refusal must name the cap it applied");
                assert_eq!(
                    line_bytes, 17,
                    "the refusal must report the line length it saw, so a log \
                     line can say how far over the producer was"
                );
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    /// The `/dev/zero` shape at the line level, and the whole point of the
    /// function: a peer that sends bytes and never a newline. Under
    /// `tokio::io::AsyncBufReadExt::lines` this allocates until the process
    /// dies.
    #[tokio::test]
    async fn read_capped_line_refuses_an_endless_newline_free_stream() {
        let mut reader = tokio::io::BufReader::new(tokio::io::repeat(b'z'));
        let err = line_within_timeout(read_capped_line(&mut reader, 4096))
            .await
            .expect_err("a stream with no newline in it must be refused");
        match err {
            CappedLineError::TooLong { limit, line_bytes } => {
                assert_eq!(limit, 4096);
                assert!(
                    line_bytes > 4096,
                    "the refusal fires on the first chunk that carries the line \
                     past the cap: {line_bytes}"
                );
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    /// The cap is **per line**, not per connection: one long-but-legal line
    /// must not poison the ones after it. This is what makes the bound safe to
    /// apply to a socket a producer keeps open across many events.
    #[tokio::test]
    async fn read_capped_line_reads_successive_lines_from_one_reader() {
        let mut reader = line_reader(b"first\nsecond\nthird\n");
        for expected in ["first", "second", "third"] {
            let got = line_within_timeout(read_capped_line(&mut reader, 16))
                .await
                .expect("each line is under the cap");
            assert_eq!(got.as_deref(), Some(expected));
        }
        let end = line_within_timeout(read_capped_line(&mut reader, 16))
            .await
            .expect("a clean EOF is not an error");
        assert_eq!(
            end, None,
            "EOF with nothing buffered must report end-of-stream, which is how \
             the hook loop learns a fire-and-forget send is over"
        );
    }

    /// `tokio::io::Lines` yields a trailing partial line at EOF, and a producer
    /// that writes its JSON and closes without a newline relies on that. The
    /// bound must not change it.
    #[tokio::test]
    async fn read_capped_line_returns_a_trailing_line_with_no_newline() {
        let mut reader = line_reader(b"{\"a\":1}");
        let got = line_within_timeout(read_capped_line(&mut reader, 64))
            .await
            .expect("a newline-less final line is still a line");
        assert_eq!(got.as_deref(), Some("{\"a\":1}"));
    }

    /// CRLF parity with `Lines`, which strips the `\r` as well as the `\n`.
    #[tokio::test]
    async fn read_capped_line_strips_a_crlf_carriage_return() {
        let mut reader = line_reader(b"hello\r\n");
        let got = line_within_timeout(read_capped_line(&mut reader, 64))
            .await
            .expect("a CRLF line is a line");
        assert_eq!(got.as_deref(), Some("hello"));
    }

    /// Non-UTF-8 is reported as `InvalidData`, exactly as `Lines` did — so the
    /// rewrite does not change how a garbage payload is treated (the hook loop
    /// dropped the connection on it before, and still does).
    #[tokio::test]
    async fn read_capped_line_reports_invalid_utf8_as_an_io_error() {
        let mut reader = line_reader(b"\xff\xfe\n");
        let err = line_within_timeout(read_capped_line(&mut reader, 64))
            .await
            .expect_err("invalid UTF-8 must not be returned as a line");
        match err {
            CappedLineError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidData),
            other => panic!("expected Io(InvalidData), got {other:?}"),
        }
    }

    /// The production constant is generous enough that the largest payload the
    /// project's own CLI can put on this socket clears it by a wide margin.
    /// `MAX_TASK_BYTES` of task text is the biggest legitimate field, and even
    /// with every byte of it escaped to two on the wire it is a quarter of the
    /// cap — so this is the "no legitimate producer is truncated" claim as an
    /// assertion rather than as arithmetic in a doc comment.
    #[test]
    fn max_hook_line_clears_the_largest_legitimate_payload() {
        let worst_case_escaped_task = MAX_TASK_BYTES as usize * 2;
        assert!(
            worst_case_escaped_task * 4 <= MAX_HOOK_LINE_BYTES,
            "the hook-line cap ({MAX_HOOK_LINE_BYTES}) must leave several times \
             the largest legitimate task payload ({worst_case_escaped_task} \
             bytes escaped) in headroom"
        );
    }
}
